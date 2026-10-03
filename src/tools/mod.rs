//! Tool roster (§4): schemas, the approval-gated executor, and dispatch.

pub mod ask;
pub mod custom;
pub mod debugger;
pub mod exec;
pub mod fs;
pub mod intel;
pub mod monitor;
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
    pub checkpoints: crate::checkpoint::Store,
    pub custom: custom::Registry,
    /// Session-scoped watches (`monitor` tool); the loop drains their events.
    pub monitors: crate::monitor::Manager,
    pub afk: bool,
    /// Run a post-edit LSP diagnostics pass, spawning the server on first touch
    /// (§3.1). Mirrors `Config::lsp_check_edits`; off in tests so an edit never
    /// spawns rust-analyzer/tsserver.
    pub lsp_check_edits: bool,
    /// Current task + turn, for checkpoint tagging and read dedupe.
    pub task: u32,
    pub turn: u32,
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
    pub async fn run_batch(&mut self, calls: Vec<ToolCall>, ui: &UiHandle) -> Result<Vec<Message>> {
        self.toolbox.turn += 1;
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
                steps.iter().filter_map(|s| s.get("command")?.as_str()).any(|command| {
                    crate::shell::pipe_segments(command)
                        .is_ok_and(|segments| segments.iter().any(|seg| !is_inspection(seg)))
                })
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

/// One-line gist of a call for the ToolStarted transcript line.
fn summarize(call: &ToolCall) -> String {
    let a = &call.arguments;
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
/// cacheable prefix (§8.4). Descriptions stay terse — they're token-billed
/// on every request; the system prompt carries the behavioral rules.
pub fn schemas(custom: &custom::Registry) -> Vec<ToolSchema> {
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
            description: "Run commands as ordered steps (one program each; pipes/redirects ok, \
                          no && ; || &). Starts at the project root."
                .into(),
            parameters: json!({"type":"object","properties":{
                "steps":{"type":"array","items":{"type":"object","properties":{
                    "command":{"type":"string"},
                    "cwd":{"type":"string","description":"directory for this step (replaces cd)"},
                    "env":{"type":"object","additionalProperties":{"type":"string"},"description":"env for this step (replaces export)"},
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
            checkpoints: crate::checkpoint::Store::open(dir, uuid::Uuid::now_v7()).unwrap(),
            custom: custom::Registry { entries: vec![] },
            monitors: crate::monitor::Manager::new(sandbox.clone()).0,
            sandbox,
            afk: false,
            lsp_check_edits: false,
            task: 1,
            turn: 1,
        };
        (tb, UiHandle { agent: ROOT, tx }, rx)
    }
}


