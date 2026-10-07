//! Tool roster (§4): schemas, the approval-gated executor, and dispatch.

pub mod agent;
pub mod decide;
pub mod ask;
pub mod custom;
pub mod debugger;
pub mod exec;
pub mod fs;
pub mod intel;
pub mod monitor;
pub mod normalize;
pub mod profile;
pub mod rizin;
pub mod search;

use anyhow::Result;
use std::path::PathBuf;

use crate::api::{Message, ToolCall, ToolSchema};
use crate::bus::{AgentId, UiHandle};

/// Everything the tools need — owned here, handed to the Executor, never
/// global (§5.7). One Toolbox per agent.
pub struct Toolbox {
    pub agent: AgentId,
    pub project: PathBuf,
    pub fs: fs::State,
    pub sandbox: crate::sandbox::Sandbox,
    pub lsp: crate::lsp::Manager,
    pub debugger: Option<crate::dap::Session>,
    pub rizin: Option<crate::rizin::Session>,
    /// Shared with subagents: every agent's file changes, tool-made or
    /// shell-made, go through one recorder (§3.3).
    pub changes: std::sync::Arc<std::sync::Mutex<crate::changes::Changes>>,
    pub custom: custom::Registry,
    /// Tools this agent may call (None = all). Subagents get a reader's or
    /// a writer's set (§5.7).
    pub mask: Option<&'static [&'static str]>,
    /// Areas this agent may write: None for the root agent, empty for a
    /// reader; a fork's subtasks must stay inside them.
    pub writes: Option<Vec<String>>,
    /// Tools withheld from an otherwise unmasked agent — the lead's
    /// `edit`/`write` when it works through subagents (§5.7).
    pub denied: &'static [&'static str],
    /// Spawns subagents (§5.7). None in a subagent: no nesting.
    pub subagents: Option<std::sync::Arc<crate::agent::Spawner>>,
    /// Session-scoped watches (`monitor` tool); the loop drains their events.
    pub monitors: crate::monitor::Manager,
    /// Decision model for `decide` and the shadow gate (None = not configured).
    pub decider: Option<std::sync::Arc<crate::decide::Decider>>,
    pub afk: bool,
    /// Run a post-edit LSP diagnostics pass, spawning the server on first touch
    /// (§3.1). Mirrors `Config::lsp_check_edits`; off in tests so an edit never
    /// spawns rust-analyzer/tsserver.
    pub lsp_check_edits: bool,
    /// Current task + turn, for change attribution and read dedupe.
    pub task: u32,
    pub turn: u32,
    /// Set by `split` (§5.8): the loop ends the task and hands back what was
    /// finished and what remains.
    pub split: Option<(String, String)>,
    /// Set by `fork` (§5.8): the loop ends the task; the pool runs these in
    /// parallel and continues with their reports.
    pub fork: Option<ForkSpec>,
}

pub struct ForkSpec {
    pub done: String,
    pub tasks: Vec<crate::deals::pool::ChildSpec>,
    pub then: String,
}

pub struct Executor {
    pub toolbox: Toolbox,
}

impl Executor {
    /// Results return as ToolResult messages in call order — providers
    /// require all ids answered. Tool failures become error results, never
    /// batch failures: the loop continues and the model adapts (§3.4).
    ///
    /// TODO(§5.2 parallel): fan out read-only calls once Toolbox splits the
    /// read-tracker (which `read` mutates) from truly shared state; execution
    /// is serial today, in call order.
    pub async fn run_batch(&mut self, mut calls: Vec<ToolCall>, ui: &UiHandle) -> Result<Vec<Message>> {
        self.toolbox.turn += 1;
        for call in &mut calls {
            normalize::canonicalize(call);
        }
        let mut results = Vec::with_capacity(calls.len());
        for call in &calls {
            ui.send(crate::bus::EventKind::ToolStarted {
                name: call.name.clone(),
                summary: summarize(call),
            })
            .await;
            let (content, is_error) = match self.run_one(call, ui).await {
                Ok(content) => (content, false),
                Err(e) => (format!("{e:#}"), true),
            };
            // Context accounting (§4): what each tool puts into the transcript.
            tracing::info!(
                tool = %call.name,
                args_bytes = call.arguments.to_string().len(),
                result_bytes = content.len(),
                is_error,
                "tool result"
            );
            tracing::debug!(tool = %call.name, args = %call.arguments, "tool args");
            ui.send(crate::bus::EventKind::ToolFinished { name: call.name.clone(), content: content.clone(), is_error })
                .await;
            results.push(Message::ToolResult {
                call_id: call.id.clone(),
                content,
                is_error,
            });
        }
        Ok(results)
    }

    /// Dispatch one call — this match is the tool wiring of record.
    async fn run_one(&mut self, call: &ToolCall, ui: &UiHandle) -> Result<String> {
        if let Some(problem) = &call.malformed {
            anyhow::bail!("{problem}");
        }
        if self.toolbox.mask.is_some_and(|m| !m.contains(&call.name.as_str())) {
            anyhow::bail!("{} is not available to this agent", call.name);
        }
        if self.toolbox.denied.contains(&call.name.as_str()) {
            anyhow::bail!(
                "{} is not yours: you lead, subagents edit. Delegate this change with `agent`, the files it \
                 changes in `writes`, and a brief that names the exact change and what to run; or follow up with \
                 the subagent that already has the context (agent: \"agent-N\")",
                call.name
            );
        }
        match call.name.as_str() {
            "read" => fs::read(&mut self.toolbox, &call.arguments).await,
            "write" => fs::write(&mut self.toolbox, &call.arguments, ui).await,
            "edit" => fs::edit(&mut self.toolbox, &call.arguments, ui).await,
            "search" => search::search(&self.toolbox, &call.arguments).await,
            "log_search" => search::log_search(&self.toolbox, &call.arguments).await,
            "execute_command" => exec::run(&mut self.toolbox, &call.arguments, ui).await,
            "profile" => profile::run(&mut self.toolbox, &call.arguments).await,
            "debug" => debugger::run(&mut self.toolbox, &call.arguments).await,
            "rizin" => rizin::run(&mut self.toolbox, &call.arguments).await,
            "code_intel" => intel::run(&mut self.toolbox, &call.arguments).await,
            "ask_user" => ask::run(&self.toolbox, &call.arguments, ui).await,
            "monitor" => monitor::run(&mut self.toolbox, &call.arguments, ui).await,
            "agent" => agent::run(&mut self.toolbox, &call.arguments, ui).await,
            "tasks" => match &self.toolbox.subagents {
                Some(spawner) => Ok(spawner.look().await),
                None => anyhow::bail!("only the root agent files tasks"),
            },
            "decide" => decide::run(&mut self.toolbox, &call.arguments).await,
            "split" => split(&mut self.toolbox, &call.arguments),
            "fork" => fork(&mut self.toolbox, &call.arguments).await,
            other => custom::run(&mut self.toolbox, other, &call.arguments).await,
        }
    }
}

/// Parallelizable in the loop (§5.2) — and, verbatim, the subagent capability
/// mask (§5.7). ask_user is non-mutating but interactive: serial, so overlays
/// never race. Unused until run_batch fans out / subagents land; both consume
/// exactly this.
#[allow(dead_code)]
pub fn is_read_only(tool: &str) -> bool {
    matches!(tool, "read" | "search" | "log_search" | "code_intel")
}

/// Programs that only look at things: running them doesn't exercise an edit.
const INSPECTION_PROGRAMS: &[&str] = &[
    "ls", "cat", "head", "tail", "less", "grep", "egrep", "rg", "find", "fd", "wc", "echo",
    "printf", "pwd", "which", "type", "file", "stat", "tree", "du", "df", "sed", "awk", "sort",
    "uniq", "cut", "diff", "cd", "export", "true",
];
const GIT_INSPECTION: &[&str] = &["status", "diff", "log", "show", "blame", "grep", "branch"];

/// Does this call run code that could exercise an edit — the run-before-done
/// gate (§5.3)? Looking at files (ls, cat, grep, git status/diff, …) doesn't
/// count; a pipeline counts if any stage runs something else.
pub fn exercises_change(call: &ToolCall) -> bool {
    match call.name.as_str() {
        "profile" | "debug" => true,
        "execute_command" => call
            .arguments
            .get("steps")
            .and_then(serde_json::Value::as_array)
            .is_some_and(|steps| {
                steps
                    .iter()
                    .filter_map(|s| s.get("command")?.as_str())
                    .any(|command| crate::shell::segments(command).iter().any(|seg| !is_inspection(seg)))
            }),
        _ => false,
    }
}

fn is_inspection(segment: &str) -> bool {
    let Some(program) = crate::shell::program(segment) else { return true };
    let program = program.rsplit('/').next().unwrap_or(program);
    if program == "git" {
        return segment.split_whitespace().nth(1).is_some_and(|sub| GIT_INSPECTION.contains(&sub));
    }
    INSPECTION_PROGRAMS.contains(&program)
}

/// `split` (§5.8): a subagent hands back the rest of its brief. The loop
/// ends the task after this batch; the pool queues a continuation.
fn split(tb: &mut Toolbox, args: &serde_json::Value) -> Result<String> {
    let field = |k: &str| args.get(k).and_then(|v| v.as_str()).map(str::trim).unwrap_or("").to_string();
    let (done, remaining) = (field("done"), field("remaining"));
    if done.is_empty() || remaining.is_empty() {
        anyhow::bail!("give both `done` (what you finished, with file:line) and `remaining` (what is left, as a brief)");
    }
    tb.split = Some((done, remaining));
    Ok("Handed back — stop here; your turn ends with this call.".into())
}

/// Write areas as declared to `agent` or `fork`, made project-relative and
/// tidy (`./src/net/` → `src/net`). `.` is the whole project, allowed only
/// where `whole`; anything outside the project is refused.
pub(crate) fn areas(project: &std::path::Path, list: &[String], whole: bool) -> Result<Vec<String>> {
    let mut out = Vec::new();
    for w in list {
        let w = w.trim();
        let rel = std::path::Path::new(w).strip_prefix(project).map(|p| p.display().to_string()).unwrap_or_else(|_| w.to_string());
        let rel = rel.trim_start_matches("./").trim_end_matches('/');
        let rel = if rel.is_empty() || rel == "." { "." } else { rel };
        if rel.starts_with('/') || rel.split('/').any(|c| c == "..") {
            anyhow::bail!("`{w}` is not inside the project");
        }
        if rel == "." && !whole {
            anyhow::bail!("`{w}` is the whole project, which leaves nothing to run alongside it — name the files or directories");
        }
        if !out.iter().any(|o| o == rel) {
            out.push(rel.to_string());
        }
    }
    // The whole project subsumes everything else.
    if out.iter().any(|o| o == ".") {
        out = vec![".".to_string()];
    }
    Ok(out)
}

/// `a` contains `b`, or `b` contains `a`.
fn overlaps(a: &str, b: &str) -> bool {
    a == "." || b == "." || a == b || b.starts_with(&format!("{a}/")) || a.starts_with(&format!("{b}/"))
}

/// `fork` (§5.8, an extension of DEALS): independent parts of the brief run
/// in parallel, then a continuation picks up with their reports. Refused
/// unless the parts look independent: no two write the same area, each
/// writes only inside the forking agent's own areas, and the decision model
/// (when configured) agrees that none needs another's result. Each subtask
/// is labelled like any task (`deals::labels`).
async fn fork(tb: &mut Toolbox, args: &serde_json::Value) -> Result<String> {
    #[derive(serde::Deserialize)]
    struct Part {
        brief: String,
        #[serde(default)]
        writes: Vec<String>,
    }
    #[derive(serde::Deserialize)]
    struct Args {
        #[serde(default)]
        done: String,
        tasks: Vec<Part>,
        then: String,
    }
    let args: Args = serde_json::from_value(args.clone())?;
    if args.tasks.len() < 2 {
        anyhow::bail!("a fork needs at least two tasks — for one, use split or do it yourself");
    }
    if args.tasks.len() > 8 {
        anyhow::bail!("at most 8 tasks per fork — group related pieces into fewer tasks");
    }
    if args.then.trim().is_empty() {
        anyhow::bail!("say in `then` what the continuation does with the results (integrate, run the full suite, …)");
    }
    let mine = tb.writes.clone().unwrap_or_else(|| vec![".".to_string()]);
    let mut tasks = Vec::new();
    for (n, part) in args.tasks.into_iter().enumerate() {
        if part.brief.trim().chars().count() < 30 {
            anyhow::bail!("task {}: the brief is too short to stand alone — say what to do, where, and what to report", n + 1);
        }
        let writes = areas(&tb.project, &part.writes, false).map_err(|e| anyhow::anyhow!("task {}: {e}", n + 1))?;
        for w in &writes {
            let inside = mine.iter().any(|m| m == "." || m == w || w.starts_with(&format!("{m}/")));
            if !inside {
                anyhow::bail!(
                    "task {} writes `{w}`, outside what you may change ({}) — a subtask can only change what you can",
                    n + 1,
                    if mine.is_empty() { "nothing: you are read-only".to_string() } else { mine.join(", ") }
                );
            }
        }
        tasks.push(crate::deals::pool::ChildSpec { brief: part.brief, writes, labels: Default::default() });
    }
    for i in 0..tasks.len() {
        for j in i + 1..tasks.len() {
            for a in &tasks[i].writes {
                if let Some(b) = tasks[j].writes.iter().find(|b| overlaps(a, b)) {
                    anyhow::bail!(
                        "tasks {} and {} both write `{}` — give that area to one task, do it before forking, or split in order",
                        i + 1,
                        j + 1,
                        if a.len() <= b.len() { a } else { b }
                    );
                }
            }
        }
    }
    if let Some(decider) = tb.decider.clone() {
        let state = serde_json::json!({
            "then": args.then,
            "tasks": tasks.iter().enumerate().map(|(n, t)| serde_json::json!({
                "n": n + 1, "brief": t.brief.chars().take(600).collect::<String>(), "writes": t.writes,
            })).collect::<Vec<_>>(),
        });
        let question = crate::decide::Question::noul(
            "These subtasks are about to run at the same time, each by a separate agent that cannot see the others' \
             work until all have finished. Can every one of them be completed without first needing another one's \
             result or changes?",
        );
        // Every subtask is labelled while independence is judged.
        let labelling: Vec<_> = tasks
            .iter()
            .map(|t| {
                let (decider, brief) = (decider.clone(), t.brief.clone());
                tokio::spawn(async move { crate::deals::labels::label(Some(&decider), &brief).await })
            })
            .collect();
        let independent = decider.ask(state, vec![("independent".to_string(), question)]).await;
        for (t, labels) in tasks.iter_mut().zip(labelling) {
            t.labels = labels.await.unwrap_or_default();
        }
        if let Ok(d) = independent {
            decider.record("fork", &format!("{} tasks", tasks.len()), &d);
            let p = d.answers.get("independent").and_then(|a| a.noul).unwrap_or(1.0);
            if p < 0.35 {
                anyhow::bail!(
                    "the decision model judged these tasks dependent on each other ({p:.2}) — do dependent work in order \
                     (split, or do it first), and fork only the parts that need nothing from each other"
                );
            }
        }
    }
    let n = tasks.len();
    tb.fork = Some(ForkSpec { done: args.done, tasks, then: args.then });
    Ok(format!("Forked {n} tasks — stop here; a continuation picks up with their reports."))
}

/// One-line gist of a call for the ToolStarted transcript line (and a
/// trajectory step in DEALS memory).
pub(crate) fn summarize(call: &ToolCall) -> String {
    let a = &call.arguments;
    if call.name == "agent" {
        let writes = a.get("writes").and_then(|v| v.as_array()).is_some_and(|w| !w.is_empty());
        let role = a.get("agent").and_then(|v| v.as_str()).unwrap_or(if writes { "writer" } else { "reader" });
        let brief: String = a.get("brief").and_then(|v| v.as_str()).unwrap_or("").lines().next().unwrap_or("").chars().take(70).collect();
        return format!("{role}: {brief}");
    }
    let candidates = [
        a.pointer("/steps/0/command"),
        a.pointer("/reads/0/file"),
        a.pointer("/edits/0/file"),
        a.pointer("/queries/0/symbol"),
        a.pointer("/actions/0/action"),
        a.get("file"),
        a.get("label"),
        a.get("stop"),
        a.get("command"),
        a.get("pattern"),
        a.get("question"),
    ];
    for c in candidates.into_iter().flatten() {
        if let Some(s) = c.as_str() {
            return s.chars().take(60).collect();
        }
    }
    String::new()
}

/// Built-in schemas + custom registry entries, in a stable order for the
/// cacheable prefix (§8.4), filtered to `mask` for subagents. Descriptions
/// stay terse — they're token-billed on every request; the system prompt
/// carries the behavioral rules.
pub fn schemas(custom: &custom::Registry, mask: Option<&[&str]>, denied: &[&str]) -> Vec<ToolSchema> {
    let mut all = all_schemas(custom);
    if let Some(mask) = mask {
        all.retain(|s| mask.contains(&s.name.as_str()));
    }
    all.retain(|s| !denied.contains(&s.name.as_str()));
    all
}

fn all_schemas(custom: &custom::Registry) -> Vec<ToolSchema> {
    use serde_json::json;
    let mut out = vec![
        ToolSchema {
            name: "read".into(),
            description: "Read files. Batched: multiple files/ranges per call. Line-numbered; \
                          records staleness for later edits. Unchanged re-reads return a \
                          reference instead of bytes."
                .into(),
            parameters: json!({"type":"object","properties":{"reads":{"type":"array","items":{
                "type":"object","properties":{
                    "file":{"type":"string"},
                    "offset":{"type":"integer","description":"1-based first line"},
                    "limit":{"type":"integer","description":"max lines"}},
                "required":["file"]}}},"required":["reads"]}),
        },
        ToolSchema {
            name: "write".into(),
            description: "Create a file, or fully rewrite one already read this session. \
                          Prefer edit for changes to existing files."
                .into(),
            parameters: json!({"type":"object","properties":{
                "file":{"type":"string"},"content":{"type":"string"}},
                "required":["file","content"]}),
        },
        ToolSchema {
            name: "edit".into(),
            description: "Exact-string edits. Each old_string must match its file uniquely \
                          (or set replace_all). Hunks for one file apply as one transaction."
                .into(),
            parameters: json!({"type":"object","properties":{"edits":{"type":"array","items":{
                "type":"object","properties":{
                    "file":{"type":"string"},
                    "old_string":{"type":"string"},
                    "new_string":{"type":"string"},
                    "replace_all":{"type":"boolean"}},
                "required":["file","old_string","new_string"]}}},"required":["edits"]}),
        },
        ToolSchema {
            name: "search".into(),
            description: "ripgrep over the project: structured file:line results, capped. \
                          glob without pattern = find files."
                .into(),
            parameters: json!({"type":"object","properties":{
                "pattern":{"type":"string"},
                "path":{"type":"string"},
                "glob":{"type":"string"},
                "max_results":{"type":"integer"}}}),
        },
        ToolSchema {
            name: "execute_command".into(),
            description: "Run shell commands as ordered steps. Each step is a shell script (chains, \
                          pipes, redirects, $(…), heredocs ok; no trailing &, use background). One \
                          shell per call, starting at the project root."
                .into(),
            parameters: json!({"type":"object","properties":{
                "steps":{"type":"array","items":{"type":"object","properties":{
                    "command":{"type":"string"},
                    "cwd":{"type":"string","description":"directory for this step only"},
                    "env":{"type":"object","additionalProperties":{"type":"string"},"description":"env for this step only"},
                    "streams":{"enum":["auto","none","stdout","stderr","both"],"default":"auto",
                        "description":"auto: stdout on success, error extract on failure"},
                    "timeout_seconds":{"type":"integer","default":30},
                    "tail_lines":{"type":"integer","default":20},
                    "network":{"enum":["full"],"description":"hosts beyond package registries; asks the user, this call only"}},
                    "required":["command"]}},
                "on_error":{"enum":["stop","continue"],"default":"stop"},
                "background":{"type":"boolean","description":"run in the background: returns at once with a monitor id; you're woken when it exits, with the output tail — for long builds/tests while you keep working"},
                "label":{"type":"string","description":"name for a background job"}},
                "required":["steps"]}),
        },
        ToolSchema {
            name: "profile".into(),
            description: "Measure performance. Modes: time (repeated runs, optional baseline \
                          command for an A/B delta), counters (perf stat), hotspots (top \
                          functions + flamegraph), allocs, syscalls."
                .into(),
            parameters: json!({"type":"object","properties":{
                "command":{"type":"string"},
                "mode":{"enum":["time","counters","hotspots","allocs","syscalls"]},
                "runs":{"type":"integer"},
                "baseline":{"type":"string"}},
                "required":["command","mode"]}),
        },
        ToolSchema {
            name: "debug".into(),
            description: "gdb session; actions run in order, stop on first error. \
                          launch{program,args?,record?,stop_at?:main|entry} attach{pid} open_core{program,core} \
                          break{location,condition?} (function, file:line, or 0xADDR) watch{expr} \
                          continue|step|next|finish|rcontinue|rstep{timeout_seconds?} \
                          stack{max_frames?} locals{frame?} eval{expr,frame?} registers \
                          read_memory{addr,len} dump_memory{addr,len,path} disassemble{at,count?} \
                          command{text} quit. Resumes return compact stop reports."
                .into(),
            parameters: json!({"type":"object","properties":{
                "actions":{"type":"array","items":{"type":"object"}}},
                "required":["actions"]}),
        },
        ToolSchema {
            name: "rizin".into(),
            description: "Reverse-engineer a binary with rizin (static analysis — does NOT \
                          run it). Persistent analyzed session: `open` a binary once (add \
                          deep:true for `aaa`), then issue raw rizin commands. Useful: afl \
                          (functions), pd N @ addr / pdf @ fcn (disassemble), iz/izz \
                          (strings), ii (imports), is (symbols), axt addr (xrefs to), px N @ \
                          addr (hexdump), s addr (seek). Works well on stripped/static \
                          binaries where the debugger struggles."
                .into(),
            parameters: json!({"type":"object","properties":{
                "open":{"type":"string","description":"binary path to open+analyze"},
                "deep":{"type":"boolean","description":"aaa instead of aa"},
                "commands":{"type":"array","items":{"type":"string"}},
                "close":{"type":"boolean"},
                "tail_lines":{"type":"integer"}}}),
        },
        ToolSchema {
            name: "code_intel".into(),
            description: "Semantic queries via the language server. Address symbols by NAME; \
                          file/line/col only to disambiguate. Batched."
                .into(),
            parameters: json!({"type":"object","properties":{"queries":{"type":"array","items":{
                "type":"object","properties":{
                    "action":{"enum":["definition","hover","references","diagnostics"]},
                    "symbol":{"type":"string"},
                    "file":{"type":"string"},
                    "line":{"type":"integer"},
                    "col":{"type":"integer"}},
                "required":["action"]}}},"required":["queries"]}),
        },
        ToolSchema {
            name: "log_search".into(),
            description: "Grep the full session log — everything truncated from tool results \
                          is recoverable here; log#N ids locate specific outputs."
                .into(),
            parameters: json!({"type":"object","properties":{
                "pattern":{"type":"string"},
                "context_lines":{"type":"integer"}},
                "required":["pattern"]}),
        },
        ToolSchema {
            name: "ask_user".into(),
            description: "Ask the user ONE question when blocked on a decision only they can \
                          make. Optional list of options."
                .into(),
            parameters: json!({"type":"object","properties":{
                "question":{"type":"string"},
                "options":{"type":"array","items":{"type":"string"}}},
                "required":["question"]}),
        },
        ToolSchema {
            name: "agent".into(),
            description: "Delegate to a subagent; you get back only its report. If the task changes files, list \
                          them (or their directories) in `writes`, `.` for anywhere; without `writes` the subagent \
                          can read, search, build and run but not change the project. The harness works out what \
                          kind of task it is, picks the model, and queues tasks when every slot is busy. Returns at \
                          once; the report arrives as a message when the child finishes, so launch every \
                          independent piece together and keep working; end your turn when you need the reports. A \
                          new child knows nothing of this conversation — the brief must stand alone. To follow up \
                          with an earlier child, its context intact, pass agent:\"agent-N\" (and `writes` to change \
                          what it may write)."
                .into(),
            parameters: json!({"type":"object","properties":{
                "writes":{"type":"array","items":{"type":"string"},"description":"files or directories it may change; omit for read-only"},
                "agent":{"type":"string","description":"agent-N to continue an earlier subagent"},
                "needs":{"type":"array","items":{"enum":["vision","reasoning","long_context"]},"description":"only when the task truly requires it"},
                "brief":{"type":"string"}},
                "required":["brief"]}),
        },
        ToolSchema {
            name: "tasks".into(),
            description: "See the task pipeline: every task you filed, where it is queued or running and on which \
                          model, what it may write, and how finished ones went. Reports arrive by themselves: \
                          to wait for them, end your turn. Asked again with nothing changed, it waits up to a minute \
                          for a change."
                .into(),
            parameters: json!({"type":"object","properties":{}}),
        },
        ToolSchema {
            name: "split".into(),
            description: "Hand back the rest of your brief: say what you finished and what remains. You stop; a \
                          fresh subagent continues from your results. Use it when the remaining part is a separate \
                          job or your context is getting long — not to skip hard parts."
                .into(),
            parameters: json!({"type":"object","properties":{
                "done":{"type":"string","description":"what you finished, with files and results"},
                "remaining":{"type":"string","description":"what is left, written as a brief"}},
                "required":["done","remaining"]}),
        },
        ToolSchema {
            name: "fork".into(),
            description: "Run independent parts of your brief in parallel, then continue: each task goes to its own \
                          subagent at the same time; when all have finished, a fresh subagent picks up with their \
                          reports and `then`. Only for parts that need nothing from each other. Each brief must stand \
                          alone. A task that changes files lists the files or directories it writes in `writes`, \
                          inside what you may write; no two tasks may write the same area. You stop when you call it."
                .into(),
            parameters: json!({"type":"object","properties":{
                "done":{"type":"string","description":"what you finished first, if anything"},
                "tasks":{"type":"array","items":{"type":"object","properties":{
                    "brief":{"type":"string"},
                    "writes":{"type":"array","items":{"type":"string"},"description":"files or directories this task writes; omit for read-only"}},
                    "required":["brief"]}},
                "then":{"type":"string","description":"what the continuation does with the results"}},
                "required":["tasks","then"]}),
        },
        ToolSchema {
            name: "decide".into(),
            description: "Calibrated probabilities from a fast decision model for questions with a \
                          fixed answer set: pick among options, yes/no, or rate on a scale. Put the \
                          facts in context — it sees nothing else. Cheap (<1 s). Advice, not authority."
                .into(),
            parameters: json!({"type":"object","properties":{
                "context":{"description":"the facts, as text or an object"},
                "questions":{"type":"array","items":{"type":"object","properties":{
                    "id":{"type":"string"},
                    "question":{"type":"string"},
                    "options":{"description":"list of names, or {name: description} — makes it a choice"},
                    "scale":{"type":"array","items":{"type":"string"},"description":"ordered levels — makes it a rating"}},
                    "required":["question"]}}},
                "required":["context","questions"]}),
        },
        ToolSchema {
            name: "monitor".into(),
            description: "Wait without polling: arm a watch, end your turn, and you are woken \
                          when it fires — files changing under the project (watch:\"paths\"), or a \
                          NEW long-running command you start here whose output/exit you want \
                          streamed (watch:\"command\", e.g. tail -f). A background execute_command \
                          needs no monitor: its exit reaches you by itself. Monitors outlive the \
                          task; {stop:id} removes one, {list:true} shows them."
                .into(),
            parameters: json!({"type":"object","properties":{
                "watch":{"enum":["paths","command"]},
                "paths":{"type":"array","items":{"type":"string"},"description":"files or directories (recursive)"},
                "pattern":{"type":"string","description":"file-name wildcard, e.g. *.md"},
                "command":{"type":"string"},
                "label":{"type":"string"},
                "timeout_seconds":{"type":"integer"},
                "stop":{"type":"string"},
                "list":{"type":"boolean"}}}),
        },
    ];
    out.extend(custom.entries.iter().map(|t| t.schema.clone()));
    out
}

#[cfg(test)]
mod tests {

    #[tokio::test]
    async fn fork_refuses_overlap_and_writes_beyond_its_own_and_accepts_a_clean_split() {
        let dir = testutil::tmp("fork-tool");
        let (mut tb, _ui, _rx) = testutil::toolbox(&dir);
        let brief = "a brief long enough to stand on its own two feet";
        let overlap = serde_json::json!({"then": "integrate", "tasks": [
            {"brief": brief, "writes": ["src/net"]},
            {"brief": brief, "writes": ["src/net/http.rs"]}]});
        let err = fork(&mut tb, &overlap).await.unwrap_err().to_string();
        assert!(err.contains("tasks 1 and 2 both write `src/net`"), "{err}");
        let whole = serde_json::json!({"then": "x", "tasks": [{"brief": brief, "writes": ["."]}, {"brief": brief}]});
        assert!(fork(&mut tb, &whole).await.unwrap_err().to_string().contains("whole project"));
        assert!(fork(&mut tb, &serde_json::json!({"then": "x", "tasks": [{"brief": brief}]})).await.is_err());
        let ok = serde_json::json!({"done": "planned", "then": "run the suite", "tasks": [
            {"brief": brief, "writes": ["./src/net/"]},
            {"brief": brief, "writes": ["src/netlify.rs"]},
            {"brief": brief}]});
        assert!(fork(&mut tb, &ok).await.unwrap().starts_with("Forked 3 tasks"));
        let f = tb.fork.take().unwrap();
        assert_eq!(f.tasks[0].writes, vec!["src/net"], "normalized");
        assert!(f.tasks[2].writes.is_empty(), "no writes: a reader");
        assert_eq!(f.then, "run the suite");
        // A subtask can change only what its parent can.
        tb.writes = Some(vec!["src".into()]);
        let outside = serde_json::json!({"then": "x", "tasks": [{"brief": brief, "writes": ["src/a.rs"]}, {"brief": brief, "writes": ["docs"]}]});
        assert!(fork(&mut tb, &outside).await.unwrap_err().to_string().contains("task 2 writes `docs`, outside"));
        tb.writes = Some(vec![]);
        let reader = serde_json::json!({"then": "x", "tasks": [{"brief": brief, "writes": ["src/a.rs"]}, {"brief": brief}]});
        assert!(fork(&mut tb, &reader).await.unwrap_err().to_string().contains("you are read-only"));
        let readers = serde_json::json!({"then": "x", "tasks": [{"brief": brief}, {"brief": brief}]});
        assert!(fork(&mut tb, &readers).await.is_ok(), "a reader may fork readers");
    }

    #[test]
    fn areas_are_project_relative_and_whole_project_subsumes() {
        let p = std::path::Path::new("/w/proj");
        let a = |l: &[&str], whole| areas(p, &l.iter().map(|s| s.to_string()).collect::<Vec<_>>(), whole);
        assert_eq!(a(&["./src/", "/w/proj/docs/a.md", "src"], false).unwrap(), vec!["src", "docs/a.md"]);
        assert_eq!(a(&["src", "."], true).unwrap(), vec!["."]);
        assert_eq!(a(&["/w/proj"], true).unwrap(), vec!["."]);
        assert!(a(&["."], false).is_err());
        assert!(a(&["../elsewhere"], true).is_err());
        assert!(a(&["/etc"], true).is_err());
    }
    use super::*;

    fn exec(commands: &[&str]) -> ToolCall {
        let steps: Vec<_> = commands.iter().map(|c| serde_json::json!({"command": c})).collect();
        ToolCall {
            id: "c".into(),
            name: "execute_command".into(),
            arguments: serde_json::json!({"steps": steps}),
            malformed: None,
        }
    }

    #[test]
    fn inspection_commands_do_not_count_as_exercising_a_change() {
        assert!(!exercises_change(&exec(&["ls src", "cat a.rs | head -5", "git diff --stat"])));
        assert!(exercises_change(&exec(&["cargo test"])));
        assert!(exercises_change(&exec(&["ls", "cat input.txt | python3 main.py"])));
        assert!(exercises_change(&exec(&["./target/debug/app --help"])));
        assert!(exercises_change(&exec(&["git stash"])));
        // Chains count by their parts: looking around then testing is a run.
        assert!(!exercises_change(&exec(&["cd src && ls; git status || true"])));
        assert!(exercises_change(&exec(&["ls && cargo test"])));
        assert!(!exercises_change(&exec(&["cat <<'EOF'\ncargo test\nEOF"])), "a heredoc body is data, not a command");
    }

    #[tokio::test]
    async fn a_masked_toolbox_refuses_tools_outside_its_set() {
        let dir = testutil::tmp("mask");
        let (mut toolbox, ui, _rx) = testutil::toolbox(&dir);
        toolbox.mask = Some(&["read", "search"]);
        let mut executor = Executor { toolbox };
        let call = ToolCall {
            id: "c".into(),
            name: "write".into(),
            arguments: serde_json::json!({"file": "x.txt", "content": "no"}),
            malformed: None,
        };
        let results = executor.run_batch(vec![call], &ui).await.unwrap();
        assert!(matches!(&results[0], Message::ToolResult { is_error: true, content, .. } if content.contains("not available to this agent")));
        assert!(!dir.join("x.txt").exists());
        // Schemas shrink to the mask; the agent tool itself is never in a child's set.
        let names: Vec<String> = schemas(&custom::Registry { entries: vec![] }, Some(&["read", "search"]), &[]).into_iter().map(|s| s.name).collect();
        assert_eq!(names, vec!["read".to_string(), "search".to_string()]);
        assert!(schemas(&custom::Registry { entries: vec![] }, None, &[]).iter().any(|s| s.name == "agent"));
        // A mask keeps only what it names, in roster order.
        let lead: Vec<String> = schemas(&custom::Registry { entries: vec![] }, Some(&["agent", "tasks", "ask_user"]), &[])
            .into_iter()
            .map(|s| s.name)
            .collect();
        assert_eq!(lead, vec!["ask_user", "agent", "tasks"]);
    }

    #[tokio::test]
    async fn a_malformed_call_is_answered_with_its_problem_and_never_runs() {
        let dir = testutil::tmp("malformed");
        let (toolbox, ui, _rx) = testutil::toolbox(&dir);
        let mut executor = Executor { toolbox };
        let mut call = exec(&["touch ran.txt"]);
        call.malformed = Some("this call was cut off".into());
        let results = executor.run_batch(vec![call], &ui).await.unwrap();
        assert!(matches!(&results[0], Message::ToolResult { is_error: true, content, .. } if content.contains("cut off")));
        assert!(!dir.join("ran.txt").exists());
    }
}

#[cfg(test)]
pub(crate) mod testutil {
    use super::*;
    use crate::bus::{ROOT, UiEvent};
    use std::path::Path;
    use tokio::sync::mpsc;

    pub fn tmp(name: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("tursi-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    pub fn toolbox(dir: &Path) -> (Toolbox, UiHandle, mpsc::Receiver<UiEvent>) {
        let (tx, rx) = mpsc::channel(8);
        let sandbox = crate::sandbox::Sandbox::for_tests(dir);
        let tb = Toolbox {
            agent: ROOT,
            project: dir.to_path_buf(),
            fs: fs::State::default(),
            lsp: crate::lsp::Manager::new(dir.to_path_buf(), Default::default(), sandbox.clone()),
            debugger: None,
        rizin: None,
            changes: std::sync::Arc::new(std::sync::Mutex::new(crate::changes::Changes::open(dir))),
            custom: custom::Registry { entries: vec![] },
            mask: None,
            writes: None,
            denied: &[],
            subagents: None,
            monitors: crate::monitor::Manager::new(sandbox.clone()).0,
            decider: None,
            sandbox,
            afk: false,
            lsp_check_edits: false,
            task: 1,
            turn: 1,
            split: None,
            fork: None,
        };
        (tb, UiHandle { agent: ROOT, tx }, rx)
    }
}


