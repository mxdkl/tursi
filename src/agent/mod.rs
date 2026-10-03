//! The core loop (§5.2). `AgentLoop` is a composable unit per §5.7:
//! everything it needs arrives through the constructor — no process-global
//! state — so spawning a subagent later is calling the constructor twice.

pub mod prompt;

use anyhow::{Context, Result};
use chrono::Utc;
use std::path::{Path, PathBuf};
use std::time::Duration;
use tokio::sync::{mpsc, watch};
use uuid::Uuid;

use crate::api::{ApiError, ChatRequest, Message, Provider, StreamEvent, Turn, Usage};
use crate::bus::{AgentId, EventKind, ROOT, UiHandle};
use crate::config::Config;
use crate::output;
use crate::plan::PlanState;
use crate::router::Router;
use crate::sandbox::{OnError, Step, Streams};
use crate::stats::{self, BudgetStatus, Ledger};
use crate::syscard::SystemCard;
use crate::tools::{self, Executor, Toolbox, custom, fs};
use std::sync::{Arc, Mutex};

/// Turns an edit may go unexercised before the loop proactively nudges the
/// model to run it (§5.3) — earlier than the at-done gate, so a task that never
/// reaches a done-claim (turn-cap-bound) still gets pushed to verify its work.
const RUN_NUDGE_AFTER: u32 = 3;
/// Total run-before-done nudges per task (proactive + at-done combined), so a
/// model that ignores them still terminates via the turn cap, never loops.
const RUN_NUDGE_BUDGET: u32 = 3;

/// Construct the root agent — shared by the TUI (`ui::run`) and headless
/// (`headless::run`) so their setup can't drift. Caller spawns `serve`.
#[allow(clippy::too_many_arguments)]
pub fn build(
    project: PathBuf,
    config: Config,
    card: &SystemCard,
    session_id: Uuid,
    sandbox: crate::sandbox::Sandbox,
    secrets: crate::config::Secrets,
    afk: bool,
    headless: bool,
    // Reattaching: restore the persisted transcript before anything runs.
    resumed: bool,
    ui: UiHandle,
    steer_rx: mpsc::Receiver<String>,
    cancel_rx: watch::Receiver<bool>,
) -> Result<AgentLoop> {
    let (monitors, monitor_rx) = crate::monitor::Manager::new(sandbox.clone());
    let prefix = prompt::prefix(
        card,
        &prompt::ProjectBlock {
            root: project.display().to_string(),
            languages: detect_languages(&project),
            verify: config.verify.clone(),
            headless,
        },
    );
    let checkpoints = Arc::new(Mutex::new(crate::checkpoint::Store::open(&project, session_id)?));
    let spawner = Arc::new(Spawner {
        project: project.clone(),
        config: config.clone(),
        secrets,
        prefix: prefix.clone(),
        session: session_id,
        sandbox: sandbox.clone(),
        checkpoints: checkpoints.clone(),
        cancel: cancel_rx.clone(),
        next_id: std::sync::atomic::AtomicU32::new(1),
    });
    let toolbox = Toolbox {
        agent: ROOT,
        project: project.clone(),
        fs: fs::State::default(),
        lsp: crate::lsp::Manager::new(project.clone(), config.lsp.clone(), sandbox.clone()),
        sandbox,
        debugger: None,
        rizin: None,
        checkpoints,
        custom: custom::Registry::load()?,
        mask: None,
        subagents: Some(spawner),
        monitors,
        afk,
        lsp_check_edits: config.lsp_check_edits,
        task: 0,
        turn: 0,
    };
    let mut agent = AgentLoop::new(
        ROOT,
        session_id,
        prefix,
        config,
        Executor { toolbox },
        Ledger::open()?,
        ui,
        steer_rx,
        cancel_rx,
    );
    agent.afk = afk;
    agent.monitor_rx = Some(monitor_rx);
    if resumed {
        let n = agent.restore()?;
        tracing::info!(session = %session_id, messages = n, "resumed session");
    }
    Ok(agent)
}

/// Cheap project-language sniff for the prompt's project block.
pub fn detect_languages(project: &Path) -> Vec<String> {
    [
        ("Cargo.toml", "rust"),
        ("pyproject.toml", "python"),
        ("uv.lock", "python"),
        ("package.json", "typescript/javascript"),
        ("go.mod", "go"),
        ("compile_commands.json", "c/c++"),
    ]
    .iter()
    .filter(|(marker, _)| project.join(marker).exists())
    .map(|(_, lang)| lang.to_string())
    .collect::<std::collections::BTreeSet<_>>()
    .into_iter()
    .collect()
}

pub struct AgentLoop {
    pub id: AgentId,
    pub transcript: Vec<Message>,
    pub router: Router,
    pub executor: Executor,
    pub ledger: Ledger,
    pub plan: PlanState,
    pub afk: bool,
    pub task: u32,
    session: Uuid,
    /// Byte-stable within the session (§8.4): core prompt + system card +
    /// project block, assembled once by the caller.
    prefix: String,
    max_turns: u32,
    /// User text typed while running; injected at the next iteration (§5.2).
    steering: mpsc::Receiver<String>,
    /// Esc flips this; the loop repairs and returns Interrupted (§5.2).
    cancel: watch::Receiver<bool>,
    /// Monitor events: injected mid-task like steering; while idle, one
    /// starts a new turn (`crate::monitor`). None in unit tests.
    monitor_rx: Option<mpsc::Receiver<crate::monitor::Event>>,
    ui: UiHandle,
    config: Config,
    /// Any file changed yet this task — gates the empty-done guard (§5.3).
    edited_this_task: bool,
    /// Something has exercised the most recent edit — the run-before-done gate
    /// (§5.3). Reset by each edit, set by each command that runs code (not
    /// ls/cat/grep-style inspection, see `tools::exercises_change`).
    ran_since_edit: bool,
    /// Consecutive turns with an edit left unverified (no command run against
    /// it) — drives the proactive run nudge (§5.3).
    unverified_edit_turns: u32,
    /// Bounded count of empty-done nudges, so a model that keeps bailing still
    /// terminates via the turn cap instead of looping (§5.3).
    empty_done_nudges: u32,
    /// Bounded count of run-before-done nudges — shared by the proactive
    /// (mid-loop) and the at-done gate, so a task can't be nudged forever (§5.3).
    run_nudges: u32,
}

/// Subagent roles (§5.7). Explore and review are read-only; a worker edits
/// through the shared checkpoint store.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Role {
    Explore,
    Review,
    Worker,
}

const READ_ONLY_TOOLS: &[&str] = &["read", "search", "code_intel", "log_search"];
const WORKER_TOOLS: &[&str] =
    &["read", "write", "edit", "search", "execute_command", "profile", "debug", "rizin", "code_intel", "log_search"];

impl Role {
    pub fn parse(s: &str) -> Result<Role> {
        match s {
            "explore" => Ok(Role::Explore),
            "review" => Ok(Role::Review),
            "worker" => Ok(Role::Worker),
            other => anyhow::bail!("unknown role {other:?} — explore, review, or worker"),
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            Role::Explore => "explore",
            Role::Review => "review",
            Role::Worker => "worker",
        }
    }

    fn tools(self) -> &'static [&'static str] {
        match self {
            Role::Explore | Role::Review => READ_ONLY_TOOLS,
            Role::Worker => WORKER_TOOLS,
        }
    }

    /// Appended to the prefix: what this child is for and how to report.
    fn addendum(self) -> &'static str {
        match self {
            Role::Explore => "## Role: explore (subagent)\nYou are a read-only research subagent. Answer the brief from the code: \
                cite files and line numbers, quote the decisive lines, and state what you could not \
                determine. Your final message is the whole report the parent agent gets — complete, \
                specific, no preamble, no suggestions to run things you cannot run.",
            Role::Review => "## Role: review (subagent)\nYou are a read-only review subagent. Judge the change the brief describes \
                against its intent: correctness first, then missed cases, then style. Cite files and \
                lines. Your final message is the whole report the parent agent gets: findings ordered \
                by severity, each with location and why it matters; say plainly if it looks right.",
            Role::Worker => "## Role: worker (subagent)\nYou implement the brief, completely and nothing more, in this project. Run \
                what you change. Your final message is the whole report the parent agent gets: what \
                you changed (files), what you ran and the result, and anything the brief left open.",
        }
    }
}

/// What a subagent needs that its parent has: config, credentials, the
/// prefix, the sandbox, the shared checkpoint store, the parent's cancel.
pub struct Spawner {
    project: PathBuf,
    config: Config,
    secrets: crate::config::Secrets,
    prefix: String,
    session: Uuid,
    sandbox: crate::sandbox::Sandbox,
    checkpoints: Arc<Mutex<crate::checkpoint::Store>>,
    cancel: watch::Receiver<bool>,
    next_id: std::sync::atomic::AtomicU32,
}

/// Longest report handed back; the child's full transcript is persisted.
const REPORT_CAP: usize = 6000;

impl Spawner {
    /// Run one subagent to completion and return its report. The child's
    /// events reach the UI under its own agent id (collapsed by default).
    pub async fn run(&self, role: Role, brief: String, parent_ui: &UiHandle) -> Result<String> {
        let n = self.next_id.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let id = AgentId(n);
        let settings = self.config.agents.get(role.name()).cloned().unwrap_or_default();
        let mut config = self.config.clone();
        if let Some(model) = settings.model {
            config.model = model;
            config.fallbacks.clear();
        }
        config.max_turns_per_task = settings.max_turns.unwrap_or(30);
        config.verify.clear(); // the parent owns verification
        let provider = Provider::for_model(&config.model, &self.secrets)?;

        let (monitors, _monitor_rx) = crate::monitor::Manager::new(self.sandbox.clone());
        let toolbox = Toolbox {
            agent: id,
            project: self.project.clone(),
            fs: fs::State::default(),
            lsp: crate::lsp::Manager::new(self.project.clone(), config.lsp.clone(), self.sandbox.clone()),
            sandbox: self.sandbox.clone(),
            debugger: None,
            rizin: None,
            checkpoints: self.checkpoints.clone(),
            custom: custom::Registry { entries: vec![] },
            mask: Some(role.tools()),
            subagents: None,
            monitors,
            afk: true,
            lsp_check_edits: config.lsp_check_edits,
            task: 0,
            turn: 0,
        };
        let (_steer_tx, steer_rx) = mpsc::channel(1);
        let ui = UiHandle { agent: id, tx: parent_ui.tx.clone() };
        let mut child = AgentLoop::new(
            id,
            self.session,
            format!("{}\n{}\n", self.prefix, role.addendum()),
            config,
            Executor { toolbox },
            Ledger::open()?,
            ui,
            steer_rx,
            self.cancel.clone(),
        );
        child.afk = true;
        let started = std::time::Instant::now();
        // The loop recurses through the tool call: box the child's future.
        let outcome = Box::pin(child.run_task(&provider, brief)).await;
        child.persist_as(&format!("agent-{n}"));
        let last_text = child
            .transcript
            .iter()
            .rev()
            .find_map(|m| match m {
                Message::Assistant { text, .. } if !text.trim().is_empty() => Some(text.trim().to_string()),
                _ => None,
            })
            .unwrap_or_default();
        let mut report = match outcome? {
            TaskOutcome::Done { summary } | TaskOutcome::NeedsUser { text: summary } => {
                if summary.trim().is_empty() { last_text } else { summary }
            }
            TaskOutcome::Interrupted => format!("(interrupted)\n{last_text}"),
            TaskOutcome::TurnLimit => format!("(turn limit reached before the brief was finished; last state:)\n{last_text}"),
            TaskOutcome::BudgetHalt => format!("(budget cap reached)\n{last_text}"),
        };
        if report.len() > REPORT_CAP {
            let cut = report.char_indices().nth(REPORT_CAP).map(|(i, _)| i).unwrap_or(report.len());
            report.truncate(cut);
            report.push_str("\n… (report truncated)");
        }
        tracing::info!(agent = n, role = role.name(), secs = started.elapsed().as_secs(), "subagent finished");
        Ok(format!("[{} agent-{n}, {}s]\n{report}", role.name(), started.elapsed().as_secs()))
    }
}

pub enum TaskOutcome {
    Done { summary: String },
    /// Attended turn end that isn't a done-claim: report or question (§5.3).
    NeedsUser { text: String },
    Interrupted,
    TurnLimit,
    BudgetHalt,
}

/// The UI's control plane for the agent task; steering and cancel have their
/// own channels (§5.2).
pub enum Command {
    Task(String),
    SetAfk(bool),
    /// `:plan` — next task is a skeleton (§5.5).
    PlanEnter,
    /// `:approve` — run the typecheck gate, start fill-in (§5.5).
    PlanApprove,
    /// `:model` pin (None unpins).
    Pin(Option<String>),
    /// `:rewind n`.
    Rewind(usize),
    /// `:monitor stop <id>`.
    MonitorStop(String),
}

impl AgentLoop {
    /// The §5.7 spawn point: brief, toolset, model, budget in — no globals.
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        id: AgentId,
        session: Uuid,
        prefix: String,
        config: Config,
        executor: Executor,
        ledger: Ledger,
        ui: UiHandle,
        steering: mpsc::Receiver<String>,
        cancel: watch::Receiver<bool>,
    ) -> AgentLoop {
        let router = Router::new(config.model.clone(), config.fallbacks.clone());
        let max_turns = config.max_turns_per_task;
        AgentLoop {
            id,
            transcript: Vec::new(),
            router,
            executor,
            ledger,
            plan: PlanState::default(),
            afk: false,
            task: 0,
            session,
            prefix,
            max_turns,
            steering,
            cancel,
            monitor_rx: None,
            ui,
            config,
            edited_this_task: false,
            ran_since_edit: false,
            unverified_edit_turns: 0,
            empty_done_nudges: 0,
            run_nudges: 0,
        }
    }

    /// The agent's home task: owns the loop, executes UI commands between
    /// tasks, reports everything back over the bus. Runs until the command
    /// channel closes (UI shutdown).
    pub async fn serve(mut self, secrets: crate::config::Secrets, mut commands: mpsc::Receiver<Command>) {
        loop {
            // Idle: a user command, or a monitor firing — which starts a turn
            // of its own with the event as the instruction.
            let command = tokio::select! {
                command = commands.recv() => match command {
                    Some(c) => c,
                    None => break,
                },
                event = async {
                    match self.monitor_rx.as_mut() {
                        Some(rx) => rx.recv().await,
                        None => std::future::pending().await,
                    }
                } => match event {
                    Some(event) => {
                        self.note_monitor_end(&event);
                        let text = event.render();
                        self.ui.send(EventKind::MonitorWoke { text: text.clone() }).await;
                        Command::Task(text)
                    }
                    None => {
                        self.monitor_rx = None;
                        continue;
                    }
                },
            };
            match command {
                Command::Task(instruction) => self.run_instruction(&secrets, instruction).await,
                Command::SetAfk(afk) => {
                    self.afk = afk;
                    self.executor.toolbox.afk = afk;
                }
                Command::PlanEnter => self.plan.enter(),
                Command::PlanApprove => {
                    let gate = crate::plan::typecheck_gate(
                        &self.executor.toolbox.sandbox,
                        &self.config.typecheck,
                    )
                    .await;
                    let summary = match gate {
                        Ok(Ok(())) => {
                            let total =
                                crate::plan::count_stubs(&self.executor.toolbox.project).unwrap_or(0);
                            self.plan.approve(total);
                            self.transcript
                                .push(Message::User(prompt::fill_in_injection().to_string()));
                            self.ui.send(EventKind::StubProgress { filled: 0, total }).await;
                            format!("skeleton approved — fill-in begins ({total} stubs)")
                        }
                        Ok(Err(red)) => format!("✗ typecheck gate red: {red}"),
                        Err(e) => format!("✗ gate failed to run: {e:#}"),
                    };
                    self.ui.send(EventKind::TaskDone { summary }).await;
                }
                Command::Pin(model) => self.router.pin(model),
                Command::MonitorStop(id) => {
                    let summary = if self.executor.toolbox.monitors.stop(&id) {
                        format!("monitor {id} stopped")
                    } else {
                        format!("✗ no monitor {id}")
                    };
                    self.send_monitors().await;
                    self.ui.send(EventKind::TaskDone { summary }).await;
                }
                Command::Rewind(n) => {
                    let summary = match self.executor.toolbox.checkpoints.lock().unwrap().rewind(n) {
                        Ok(files) if files.is_empty() => "nothing to rewind".to_string(),
                        Ok(files) => format!(
                            "rewound {} file(s): {}",
                            files.len(),
                            files
                                .iter()
                                .filter_map(|f| f.file_name().and_then(|n| n.to_str()))
                                .collect::<Vec<_>>()
                                .join(", ")
                        ),
                        Err(e) => format!("✗ rewind failed: {e:#}"),
                    };
                    self.ui.send(EventKind::TaskDone { summary }).await;
                }
            }
        }
        // Command channel closed = UI shut down cleanly. Checkpoints stay
        // (a resumed session can still `:rewind`); `Store::open` prunes by age.
    }

    /// One user instruction (or monitor wake) end to end, reported as TaskDone.
    async fn run_instruction(&mut self, secrets: &crate::config::Secrets, instruction: String) {
        self.router.reset();
        let provider = match Provider::for_model(self.router.model(), secrets) {
            Ok(p) => p,
            Err(e) => {
                self.ui.send(EventKind::TaskDone { summary: format!("✗ {e:#}") }).await;
                return;
            }
        };
        if self.plan.active {
            self.transcript.push(Message::User(prompt::plan_injection().to_string()));
        }
        let summary = match self.run_task(&provider, instruction).await {
            Ok(TaskOutcome::Done { summary }) => summary,
            Ok(TaskOutcome::NeedsUser { text }) => text,
            Ok(TaskOutcome::Interrupted) => "⏹ interrupted".to_string(),
            Ok(TaskOutcome::TurnLimit) => "✗ turn limit reached — task incomplete".to_string(),
            Ok(TaskOutcome::BudgetHalt) => "✗ budget cap reached — task halted".to_string(),
            Err(e) => format!("✗ task failed: {e:#}"),
        };
        self.persist();
        let session_usd = self.ledger.session_total(self.session).unwrap_or(0.0);
        let month_usd = self.ledger.month_total().unwrap_or(0.0);
        self.ui.send(EventKind::Cost { session_usd, month_usd }).await;
        if self.plan.approved
            && let Ok((filled, total)) = crate::plan::progress(&self.executor.toolbox.project, &self.plan)
        {
            self.ui.send(EventKind::StubProgress { filled, total }).await;
        }
        self.send_monitors().await;
        self.ui.send(EventKind::TaskDone { summary }).await;
    }

    /// Write the transcript (and task counter) to the session directory —
    /// after every turn, so `--resume` has the latest. Best-effort: a write
    /// failure is logged, never fatal.
    fn persist(&self) {
        self.persist_to(crate::session::Session::transcript_path(&self.executor.toolbox.project, self.session));
    }

    /// A subagent's transcript, beside the session's: `<session>.agent-N.transcript.json`.
    fn persist_as(&self, suffix: &str) {
        let base = crate::session::Session::transcript_path(&self.executor.toolbox.project, self.session);
        let name = format!("{}.{suffix}.transcript.json", self.session);
        self.persist_to(base.with_file_name(name));
    }

    fn persist_to(&self, path: PathBuf) {
        let doc = serde_json::json!({ "task": self.task, "messages": self.transcript });
        let tmp = path.with_extension("json.tmp");
        let write = || {
            if let Some(dir) = path.parent() {
                std::fs::create_dir_all(dir)?;
            }
            std::fs::write(&tmp, doc.to_string())?;
            std::fs::rename(&tmp, &path)
        };
        if let Err(e) = write() {
            tracing::warn!("persisting the transcript failed: {e:#}");
        }
    }

    /// Load the persisted transcript of a resumed session: unanswered calls
    /// are repaired, and a `[resumed]` note tells the model what did not
    /// survive (monitors, background jobs, its read-state). Returns the
    /// message count.
    pub fn restore(&mut self) -> Result<usize> {
        let path = crate::session::Session::transcript_path(&self.executor.toolbox.project, self.session);
        let Ok(raw) = std::fs::read_to_string(&path) else { return Ok(0) };
        let doc: serde_json::Value = serde_json::from_str(&raw).with_context(|| format!("parsing {}", path.display()))?;
        self.task = doc.get("task").and_then(|t| t.as_u64()).unwrap_or(0) as u32;
        self.executor.toolbox.task = self.task;
        self.transcript = serde_json::from_value(doc.get("messages").cloned().unwrap_or_default())?;
        self.repair_transcript("session ended before this call completed");
        if !self.transcript.is_empty() {
            self.transcript.push(Message::User(
                "[resumed] This session was resumed in a new process. Monitors and background jobs \
                 from before are gone; files may have changed since — re-read before editing. \
                 Checkpoints are intact."
                    .to_string(),
            ));
        }
        Ok(self.transcript.len())
    }

    /// A monitor that exited or timed out is gone from the list.
    fn note_monitor_end(&mut self, event: &crate::monitor::Event) {
        if matches!(event.what, crate::monitor::What::Exited { .. } | crate::monitor::What::TimedOut { .. }) {
            self.executor.toolbox.monitors.forget(&event.id);
        }
    }

    async fn send_monitors(&self) {
        let armed = self.executor.toolbox.monitors.list().into_iter().map(|(id, label, _)| (id, label)).collect();
        self.ui.send(EventKind::Monitors { armed }).await;
    }

    /// One task, §5.2 wiring — this body is the plan of record.
    pub async fn run_task(&mut self, provider: &Provider, instruction: String) -> Result<TaskOutcome> {
        self.task += 1;
        self.executor.toolbox.task = self.task;
        self.router.reset();
        self.edited_this_task = false;
        self.ran_since_edit = false;
        self.unverified_edit_turns = 0;
        self.empty_done_nudges = 0;
        self.run_nudges = 0;
        // Self-heal before appending: a transcript left with an unanswered
        // tool call (a crash or bug mid-turn) is rejected by the provider on
        // every request, so one bad turn would otherwise sink every later task.
        self.repair_transcript("interrupted — this call never completed");
        self.transcript.push(Message::User(instruction));
        self.persist();

        // No turn cap: a task runs until it's done, blocked, interrupted
        // (Esc), or a budget cap halts it (§9) — those are the real bounds.
        // A positive `max_turns` is an optional ceiling (0 = unlimited).
        let mut turn_no = 0u32;
        loop {
            turn_no += 1;
            if self.max_turns > 0 && turn_no > self.max_turns {
                return Ok(TaskOutcome::TurnLimit);
            }
            let cancelled = *self.cancel.borrow();
            if cancelled {
                self.repair_transcript("interrupted by user");
                return Ok(TaskOutcome::Interrupted);
            }
            self.inject_steering();
            if let Some(halt) = self.enforce_budgets(provider).await? {
                return Ok(halt);
            }

            let turn = self.call_model(provider).await?;
            self.transcript.push(Message::Assistant {
                text: turn.text.clone(),
                tool_calls: turn.tool_calls.clone(),
            });

            if turn.tool_calls.is_empty() {
                match self.finish_turn(&turn.text).await? {
                    Some(outcome) => return Ok(outcome),
                    // AFK verify red: [verify] message injected, keep going.
                    None => continue,
                }
            }

            // Classify this turn's calls before run_batch consumes them: file
            // mutations and code-running calls feed the run-before-done gate
            // (§5.3).
            let edit_ids: Vec<String> = turn
                .tool_calls
                .iter()
                .filter(|c| matches!(c.name.as_str(), "edit" | "write"))
                .map(|c| c.id.clone())
                .collect();
            let exec_ids: Vec<String> = turn
                .tool_calls
                .iter()
                .filter(|c| tools::exercises_change(c))
                .map(|c| c.id.clone())
                .collect();
            let results = self.executor.run_batch(turn.tool_calls, &self.ui).await?;
            // Scoped so the borrow of `results` ends before they're absorbed.
            let (productive, ran_ok) = {
                let succeeded = |ids: &[String]| {
                    results.iter().any(|m| matches!(m,
                        Message::ToolResult { call_id, is_error: false, .. }
                            if ids.iter().any(|id| id == call_id)))
                };
                (succeeded(&edit_ids), succeeded(&exec_ids))
            };
            if productive {
                self.edited_this_task = true;
                // A fresh change is unverified until something runs against it.
                self.ran_since_edit = false;
            }
            if ran_ok {
                self.ran_since_edit = true;
            }
            // Streak of turns with an edit left unexercised (§5.3).
            if self.edited_this_task && !self.ran_since_edit {
                self.unverified_edit_turns += 1;
            } else {
                self.unverified_edit_turns = 0;
            }

            self.transcript.extend(results);
            self.persist();

            // Proactive run-before-done (§5.3): edits have gone several turns
            // unexercised. Nudge to run them NOW — a cap-bound task may never
            // reach a done-claim, where the at-done gate would otherwise catch
            // it. Bounded (shared budget) and spaced (counter reset).
            if self.unverified_edit_turns >= RUN_NUDGE_AFTER && self.run_nudges < RUN_NUDGE_BUDGET {
                self.run_nudges += 1;
                self.unverified_edit_turns = 0;
                self.transcript.push(Message::User(prompt::run_before_done_nudge().to_string()));
            }
        }
    }

    /// Drain queued steering messages — and monitor events that fired during
    /// the task — into the transcript as user turns.
    fn inject_steering(&mut self) {
        while let Ok(text) = self.steering.try_recv() {
            self.transcript.push(Message::User(text));
        }
        let mut events = Vec::new();
        if let Some(rx) = self.monitor_rx.as_mut() {
            while let Ok(event) = rx.try_recv() {
                events.push(event);
            }
        }
        for event in events {
            self.note_monitor_end(&event);
            self.transcript.push(Message::User(event.render()));
        }
    }

    /// Compaction trigger (§8) and budget caps (§9). Some(BudgetHalt) stops.
    async fn enforce_budgets(&mut self, provider: &Provider) -> Result<Option<TaskOutcome>> {
        let tokens = self.context_tokens();
        if prompt::should_compact(tokens, self.config.context_window, self.config.compact_at)
            || prompt::should_compact_early(&self.transcript, tokens, self.config.compact_min_tokens)
        {
            tracing::info!(tokens, "compacting the transcript");
            let forgotten = prompt::compact(provider, &mut self.transcript).await?;
            // Their content left the context: a re-read must send bytes again,
            // not "unchanged since turn N".
            let project = self.executor.toolbox.project.clone();
            self.executor.toolbox.fs.forget(forgotten.iter().map(|f| project.join(f)));
        }
        let session_usd = self.ledger.session_total(self.session).unwrap_or(0.0);
        let month_usd = self.ledger.month_total().unwrap_or(0.0);
        self.ui.send(EventKind::Cost { session_usd, month_usd }).await;
        self.ui
            .send(EventKind::Context {
                used_tokens: self.context_tokens(),
                window: self.config.context_window,
            })
            .await;
        match stats::check(self.config.budgets, session_usd, month_usd) {
            BudgetStatus::Ok => Ok(None),
            BudgetStatus::Warn(w) => {
                tracing::warn!("{w}");
                Ok(None)
            }
            BudgetStatus::Exceeded(e) => {
                tracing::warn!("halting: {e}");
                Ok(Some(TaskOutcome::BudgetHalt))
            }
        }
    }

    /// Assemble prefix + transcript (§8.4), stream deltas to the UI, record
    /// usage. Provider errors: 2 backoff retries when a retry can help (network,
    /// 408/429/5xx — not a 400 the provider will reject again), then failover
    /// to the next configured fallback. Fallbacks must share the provider: the
    /// client is built once per task.
    async fn call_model(&mut self, provider: &Provider) -> Result<Turn> {
        let mut messages = Vec::with_capacity(self.transcript.len() + 1);
        messages.push(Message::System(self.prefix.clone()));
        messages.extend(self.transcript.iter().cloned());
        let schemas = tools::schemas(&self.executor.toolbox.custom, self.executor.toolbox.mask);

        let mut model = self.router.model().to_string();
        let mut attempt = 0u32;
        loop {
            let (events, mut rx) = mpsc::channel::<StreamEvent>(64);
            let ui = self.ui.clone();
            let forwarder = tokio::spawn(async move {
                while let Some(event) = rx.recv().await {
                    if let StreamEvent::TextDelta(t) = event {
                        ui.send(EventKind::AgentText(t)).await;
                    }
                }
            });
            let request = ChatRequest {
                model: model.clone(),
                messages: messages.clone(),
                tools: schemas.clone(),
            };
            let outcome = provider.chat(request, events).await;
            let _ = forwarder.await;
            match outcome {
                Ok(turn) => {
                    // Cost accounting is bookkeeping: a ledger write failure
                    // (bad perms, full disk) must never sink the user's work.
                    self.record_usage(&model, turn.usage);
                    return Ok(turn);
                }
                Err(e) => {
                    attempt += 1;
                    let retryable = e.downcast_ref::<ApiError>().is_none_or(ApiError::retryable);
                    if retryable && attempt <= 2 {
                        tracing::warn!(%model, attempt, "model call failed, retrying: {e:#}");
                        tokio::time::sleep(Duration::from_secs(2 * attempt as u64)).await;
                        continue;
                    }
                    match self.router.failover() {
                        Some(next) if next.split('/').next() == model.split('/').next() => {
                            tracing::warn!(from = %model, to = %next, "failing over to fallback: {e:#}");
                            model = next;
                            attempt = 0;
                        }
                        Some(next) => {
                            return Err(e).context(format!(
                                "model call failed; fallback {next} is on another provider — fallbacks must share {model}'s provider"
                            ));
                        }
                        None => return Err(e).context("model call failed after retries and fallbacks"),
                    }
                }
            }
        }
    }

    /// Best-effort: bookkeeping never fails the task (§9). A write error is
    /// logged and swallowed.
    fn record_usage(&self, model: &str, usage: Usage) {
        let cost = match self.config.prices.get(model) {
            Some(price) => stats::cost(*price, usage),
            None => {
                tracing::warn!(%model, "no price configured — cost recorded as 0");
                0.0
            }
        };
        let entry = stats::Entry {
            ts: Utc::now(),
            session: self.session,
            agent: self.id,
            task: self.task,
            model: model.to_string(),
            input_tokens: usage.input_tokens,
            cached_tokens: usage.cached_tokens,
            output_tokens: usage.output_tokens,
            cost_usd: cost,
        };
        if let Err(e) = self.ledger.record(entry) {
            tracing::warn!("cost ledger write failed (continuing): {e:#}");
        }
    }

    /// Attended: the user judges (§5.3). AFK: run the verify commands
    /// (harness-initiated, config-authored — no permission gate); green →
    /// Done, red → inject `[verify]` and return None to continue the loop.
    async fn finish_turn(&mut self, text: &str) -> Result<Option<TaskOutcome>> {
        if !self.afk {
            return Ok(Some(TaskOutcome::NeedsUser { text: text.to_string() }));
        }
        // A final turn that changed nothing AND said nothing is a bail, not a
        // completion (the empty-done regression). Nudge back to work, bounded
        // so a model that keeps bailing still terminates via the turn cap.
        if text.trim().is_empty() && !self.edited_this_task && self.empty_done_nudges < 2 {
            self.empty_done_nudges += 1;
            self.transcript.push(Message::User(prompt::empty_done_nudge().to_string()));
            return Ok(None);
        }
        // Run-before-done (always): a done that changed files but never ran
        // anything to exercise them isn't trustworthy — the model must build,
        // test, or render its change first (§5.3). Applies whether or not a
        // verify gate is configured; bounded so it can't loop.
        if self.edited_this_task && !self.ran_since_edit && self.run_nudges < RUN_NUDGE_BUDGET {
            self.run_nudges += 1;
            self.transcript.push(Message::User(prompt::run_before_done_nudge().to_string()));
            return Ok(None);
        }
        if self.config.verify.is_empty() {
            return Ok(Some(TaskOutcome::Done { summary: text.to_string() }));
        }
        self.ui.send(EventKind::Verifying).await;
        let steps = self
            .config
            .verify
            .iter()
            .map(|c| Step {
                command: c.clone(),
                cwd: None,
                env: vec![],
                streams: Streams::Auto,
                timeout: self.executor.toolbox.sandbox.timeout_cap,
                tail_lines: 30,
            })
            .collect();
        let results = self.executor.toolbox.sandbox.run_steps(steps, OnError::Stop).await?;
        match results.iter().find(|r| r.ran && r.exit_code != Some(0)) {
            None => Ok(Some(TaskOutcome::Done { summary: text.to_string() })),
            Some(red) => {
                let extract = output::truncate(&format!("{}\n{}", red.stderr, red.stdout), 30);
                let injection =
                    prompt::verify_injection(&red.command, red.exit_code.unwrap_or(-1), &extract);
                self.transcript.push(Message::User(injection));
                Ok(None)
            }
        }
    }

    /// Answer every tool call left without a result, right after its
    /// assistant message's other results — the provider rejects a request
    /// whose tool calls go unanswered (§5.2). Runs on interrupt and at every
    /// task start; a balanced transcript passes through unchanged.
    fn repair_transcript(&mut self, reason: &str) {
        let mut repaired = Vec::with_capacity(self.transcript.len());
        let mut messages = std::mem::take(&mut self.transcript).into_iter().peekable();
        while let Some(message) = messages.next() {
            let ids: Vec<String> = match &message {
                Message::Assistant { tool_calls, .. } => tool_calls.iter().map(|c| c.id.clone()).collect(),
                _ => Vec::new(),
            };
            repaired.push(message);
            let mut answered = std::collections::HashSet::new();
            while let Some(Message::ToolResult { call_id, .. }) = messages.peek() {
                answered.insert(call_id.clone());
                repaired.extend(messages.next());
            }
            for id in ids.into_iter().filter(|id| !answered.contains(id)) {
                repaired.push(Message::ToolResult { call_id: id, content: reason.to_string(), is_error: true });
            }
        }
        self.transcript = repaired;
    }

    /// Crude estimate (bytes/4) of what each request sends: the prefix, the
    /// tool schemas, and the transcript — fine for the compaction trigger.
    fn context_tokens(&self) -> u64 {
        let schemas = serde_json::to_string(&tools::schemas(&self.executor.toolbox.custom, self.executor.toolbox.mask))
            .map_or(0, |s| s.len());
        let transcript: usize = self
            .transcript
            .iter()
            .map(|m| match m {
                Message::System(s) | Message::User(s) => s.len(),
                Message::Assistant { text, tool_calls } => {
                    text.len()
                        + tool_calls
                            .iter()
                            .map(|c| c.name.len() + c.arguments.to_string().len())
                            .sum::<usize>()
                }
                Message::ToolResult { content, .. } => content.len(),
            })
            .sum();
        ((self.prefix.len() + schemas + transcript) / 4) as u64
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::openai_compat;
    use crate::bus::ROOT;
    use crate::syscard::{Capabilities, SystemCard};
    use crate::tools::testutil;

    fn test_card() -> SystemCard {
        SystemCard {
            cpu: "test cpu".into(),
            isa: vec![],
            mem: "16 GiB".into(),
            gpus: vec![],
            os: "linux (test)".into(),
            toolchains: vec![],
            caps: Capabilities {
                sandbox: true,
                perf_event_paranoid: None,
                ptrace_scope: None,
                gdb_dap: false,
                rr: false,
                profile_backends: vec![],
            },
        }
    }

    fn test_agent(dir: &std::path::Path) -> AgentLoop {
        let (tb, ui, _rx) = testutil::toolbox(dir);
        let (_steer_tx, steer_rx) = mpsc::channel(4);
        let (_cancel_tx, cancel_rx) = watch::channel(false);
        let mut config = Config::default();
        config.model = "x/y".into();
        let ledger = Ledger::open_at(dir.join("stats")).unwrap();
        AgentLoop::new(
            ROOT,
            Uuid::now_v7(),
            "p".into(),
            config,
            Executor { toolbox: tb },
            ledger,
            ui,
            steer_rx,
            cancel_rx,
        )
    }

    fn afk_agent(dir: &std::path::Path) -> AgentLoop {
        let mut agent = test_agent(dir);
        agent.afk = true;
        agent
    }

    fn two_reads() -> Message {
        Message::Assistant {
            text: String::new(),
            tool_calls: vec![
                crate::api::ToolCall { id: "a".into(), name: "read".into(), arguments: serde_json::json!({}), malformed: None },
                crate::api::ToolCall { id: "b".into(), name: "read".into(), arguments: serde_json::json!({}), malformed: None },
            ],
        }
    }

    fn result(id: &str) -> Message {
        Message::ToolResult { call_id: id.into(), content: "done".into(), is_error: false }
    }

    /// True iff every assistant `tool_call` id is answered by the tool results
    /// directly following it — the well-formedness the provider requires.
    fn transcript_is_balanced(t: &[Message]) -> bool {
        t.iter().enumerate().all(|(i, m)| match m {
            Message::Assistant { tool_calls, .. } => tool_calls.iter().all(|c| {
                t[i + 1..]
                    .iter()
                    .take_while(|m| matches!(m, Message::ToolResult { .. }))
                    .any(|m| matches!(m, Message::ToolResult { call_id, .. } if *call_id == c.id))
            }),
            _ => true,
        })
    }

    #[test]
    fn interrupt_repair_answers_every_outstanding_call_id() {
        let dir = testutil::tmp("repair");
        let mut agent = test_agent(&dir);
        agent.transcript.push(two_reads());
        agent.transcript.push(result("a"));
        agent.repair_transcript("interrupted by user");
        let repaired = agent.transcript.iter().any(|m| matches!(
            m,
            Message::ToolResult { call_id, content, is_error: true }
                if call_id == "b" && content == "interrupted by user"
        ));
        assert!(repaired);
        assert_eq!(agent.transcript.len(), 3, "answered ids are not re-answered");
    }

    #[test]
    fn repair_heals_an_unanswered_call_buried_before_later_turns() {
        // The bricked-session shape: a call left unanswered mid-transcript, then
        // a new task's user message. The provider 400s on every request until
        // the missing result sits directly after its siblings.
        let dir = testutil::tmp("repair-buried");
        let mut agent = test_agent(&dir);
        agent.transcript.push(two_reads());
        agent.transcript.push(result("a"));
        agent.transcript.push(Message::User("next task".into()));
        assert!(!transcript_is_balanced(&agent.transcript));
        agent.repair_transcript("interrupted — this call never completed");
        assert!(transcript_is_balanced(&agent.transcript));
        assert!(matches!(&agent.transcript[2], Message::ToolResult { call_id, is_error: true, .. } if call_id == "b"));
        assert!(matches!(&agent.transcript[3], Message::User(t) if t == "next task"));

        // A balanced transcript passes through untouched.
        let before = agent.transcript.len();
        agent.repair_transcript("x");
        assert_eq!(agent.transcript.len(), before);
    }

    #[test]
    fn a_persisted_transcript_restores_into_a_new_loop() {
        let dir = testutil::tmp("resume");
        let session = Uuid::now_v7();
        let make = || {
            let (tb, ui, _rx) = testutil::toolbox(&dir);
            let (_steer_tx, steer_rx) = mpsc::channel(4);
            let (_cancel_tx, cancel_rx) = watch::channel(false);
            let mut config = Config::default();
            config.model = "x/y".into();
            let ledger = Ledger::open_at(dir.join("stats")).unwrap();
            AgentLoop::new(ROOT, session, "p".into(), config, Executor { toolbox: tb }, ledger, ui, steer_rx, cancel_rx)
        };
        let mut first = make();
        first.task = 3;
        first.transcript.push(Message::User("fix the bug".into()));
        first.transcript.push(two_reads());
        first.transcript.push(result("a")); // "b" left unanswered: a crash mid-turn
        first.persist();
        drop(first);

        let mut again = make();
        let n = again.restore().unwrap();
        assert_eq!(again.task, 3, "task counter continues");
        assert!(transcript_is_balanced(&again.transcript), "unanswered call repaired");
        assert!(matches!(again.transcript.last(), Some(Message::User(t)) if t.starts_with("[resumed]")));
        assert_eq!(n, 5, "user, assistant, 2 results, [resumed]");
        // A session with nothing persisted restores to nothing, quietly.
        let mut fresh = make();
        fresh.session = Uuid::now_v7();
        assert_eq!(fresh.restore().unwrap(), 0);
    }

    #[tokio::test]
    async fn monitor_events_are_injected_mid_task_and_wake_an_idle_agent() {
        let dir = testutil::tmp("monitor-wake");
        let (tb, ui, mut rx) = testutil::toolbox(&dir);
        let (_steer_tx, steer_rx) = mpsc::channel(4);
        let (_cancel_tx, cancel_rx) = watch::channel(false);
        let (mon_tx, mon_rx) = mpsc::channel(4);
        let mut config = Config::default();
        config.model = "nokey/model".into();
        let ledger = Ledger::open_at(dir.join("stats")).unwrap();
        let mut agent = AgentLoop::new(ROOT, Uuid::now_v7(), "p".into(), config, Executor { toolbox: tb }, ledger, ui, steer_rx, cancel_rx);
        agent.monitor_rx = Some(mon_rx);
        let event = |what| crate::monitor::Event { id: "m1".into(), label: "inbox".into(), what };

        // Mid-task: drained into the transcript like steering.
        mon_tx.send(event(crate::monitor::What::Files { created: vec!["inbox/a.md".into()], modified: vec![], deleted: vec![] })).await.unwrap();
        agent.inject_steering();
        assert!(matches!(agent.transcript.last(), Some(Message::User(t)) if t == "[monitor m1 inbox] created inbox/a.md"));

        // Idle: an event starts a turn of its own (here it fails fast on the
        // missing credential — the wake and the TaskDone are what we check).
        let (cmd_tx, cmd_rx) = mpsc::channel(4);
        let secrets = crate::config::Secrets::load_from(&dir.join("nonexistent-secrets.toml")).unwrap();
        let serve = tokio::spawn(agent.serve(secrets, cmd_rx));
        mon_tx.send(event(crate::monitor::What::Exited { code: Some(0), tail: String::new() })).await.unwrap();
        let mut woke = None;
        let mut done = None;
        while let Ok(Some(ev)) = tokio::time::timeout(Duration::from_secs(5), rx.recv()).await {
            match ev.kind {
                crate::bus::EventKind::MonitorWoke { text } => woke = Some(text),
                crate::bus::EventKind::TaskDone { summary } => {
                    done = Some(summary);
                    break;
                }
                _ => {}
            }
        }
        assert_eq!(woke.as_deref(), Some("[monitor m1 inbox] finished: exit 0 — monitor removed"));
        assert!(done.is_some_and(|d| d.contains("no credentials")), "the wake ran a turn");
        drop(cmd_tx);
        let _ = tokio::time::timeout(Duration::from_secs(5), serve).await;
    }

    #[tokio::test]
    async fn empty_final_turn_with_no_edits_is_nudged_not_done() {
        let dir = testutil::tmp("emptydone");
        let mut agent = afk_agent(&dir);
        // Nothing said, nothing changed → not a completion; a [continue] nudge.
        let out = agent.finish_turn("").await.unwrap();
        assert!(out.is_none(), "empty no-edit turn must not be Done");
        assert!(matches!(agent.transcript.last(), Some(Message::User(s)) if s.contains("[continue]")));
        // A real summary is accepted even with no edits (an explanation is work).
        assert!(matches!(
            agent.finish_turn("here is what I found").await.unwrap(),
            Some(TaskOutcome::Done { .. })
        ));
        // Empty text but files were changed AND exercised → the work stands.
        agent.edited_this_task = true;
        agent.ran_since_edit = true;
        assert!(matches!(agent.finish_turn("").await.unwrap(), Some(TaskOutcome::Done { .. })));
    }

    #[tokio::test]
    async fn done_with_edits_but_nothing_run_is_gated_until_a_run() {
        let dir = testutil::tmp("rungate");
        let mut agent = afk_agent(&dir);
        agent.edited_this_task = true;
        agent.ran_since_edit = false;
        // Changed files but ran nothing against them → gated, not Done.
        let out = agent.finish_turn("done, implemented the feature").await.unwrap();
        assert!(out.is_none(), "must not accept done before the change is exercised");
        assert!(matches!(agent.transcript.last(), Some(Message::User(s)) if s.contains("[continue]")));
        // Once a command has run against the change, the same done is accepted.
        agent.ran_since_edit = true;
        assert!(matches!(
            agent.finish_turn("done, implemented the feature").await.unwrap(),
            Some(TaskOutcome::Done { .. })
        ));
    }

    #[tokio::test]
    async fn empty_done_nudge_is_bounded_so_the_loop_cannot_spin() {
        let dir = testutil::tmp("emptydone-bound");
        let mut agent = afk_agent(&dir);
        assert!(agent.finish_turn("").await.unwrap().is_none());
        assert!(agent.finish_turn("").await.unwrap().is_none());
        // Bound reached: a persistent empty-done is finally accepted.
        assert!(matches!(agent.finish_turn("").await.unwrap(), Some(TaskOutcome::Done { .. })));
    }

    /// Dogfood: fix a real bug end to end — read the failing source, edit it,
    /// let the AFK verify gate (`cargo test`) decide done. Exercises
    /// read→edit→execute→verify against a live model, the way a user would.
    /// `set -a; source .env; set +a; cargo test fixes_a_real_bug -- --ignored --nocapture`
    #[tokio::test]
    #[ignore = "live: needs DEEPSEEK_API_KEY and network"]
    async fn fixes_a_real_bug_end_to_end() {
        let key = std::env::var("DEEPSEEK_API_KEY").expect("DEEPSEEK_API_KEY not set");
        let dir = testutil::tmp("e2e-bug");
        // A tiny, dependency-free crate with an off-by-one bug and a test
        // that catches it — no network needed for `cargo test`.
        std::fs::write(dir.join("Cargo.toml"), "[package]\nname=\"buggy\"\nversion=\"0.1.0\"\nedition=\"2021\"\n").unwrap();
        std::fs::create_dir_all(dir.join("src")).unwrap();
        std::fs::write(
            dir.join("src/lib.rs"),
            "/// Sum of 1..=n. Has a bug.\n\
             pub fn triangle(n: u32) -> u32 {\n\
             \x20   let mut total = 0;\n\
             \x20   for i in 1..n { total += i; }\n\
             \x20   total\n\
             }\n\n\
             #[test]\n\
             fn triangle_of_5_is_15() {\n\
             \x20   assert_eq!(triangle(5), 15);\n\
             }\n",
        )
        .unwrap();

        let (tb, ui, mut rx) = testutil::toolbox(&dir);
        // Log the transcript so a failure is diagnosable, not a mystery.
        tokio::spawn(async move {
            while let Some(ev) = rx.recv().await {
                match ev.kind {
                    crate::bus::EventKind::ToolStarted { name, summary } => {
                        eprintln!("  ▸ {name} {summary}");
                    }
                    crate::bus::EventKind::ToolFinished { name, content, is_error } => {
                        eprintln!("  {} {name}: {}", if is_error { '✗' } else { '✓' }, content.lines().next().unwrap_or(""));
                    }
                    crate::bus::EventKind::TaskDone { summary } => eprintln!("  ● {summary}"),
                    _ => {}
                }
            }
        });

        let mut config = Config::default();
        config.model = "deepseek/deepseek-chat".into();
        config.verify = vec!["cargo test".into()];
        config.max_turns_per_task = 12;

        let prefix = prompt::prefix(
            &test_card(),
            &prompt::ProjectBlock {
                root: dir.display().to_string(),
                languages: vec!["rust".into()],
                verify: config.verify.clone(),
                headless: true,
            },
        );
        let (_steer_tx, steer_rx) = mpsc::channel(4);
        let (_cancel_tx, cancel_rx) = watch::channel(false);
        let ledger = Ledger::open_at(dir.join("stats")).unwrap();
        let mut agent = AgentLoop::new(
            ROOT, Uuid::now_v7(), prefix, config,
            Executor { toolbox: tb }, ledger,
            ui, steer_rx, cancel_rx,
        );
        agent.afk = true;

        let provider = Provider::OpenAiCompat(openai_compat::Client::new(
            key, "https://api.deepseek.com/v1".to_string(),
        ));
        let outcome = agent
            .run_task(&provider, "A test is failing in this Rust crate. Find and fix the bug, then make sure the tests pass.".to_string())
            .await
            .unwrap();

        assert!(matches!(outcome, TaskOutcome::Done { .. }), "task did not reach a green verify gate");
        // Ground truth: the fix is real, not just claimed.
        let src = std::fs::read_to_string(dir.join("src/lib.rs")).unwrap();
        assert!(src.contains("1..=n") || src.contains("1..n + 1") || src.contains("1..(n + 1)"),
            "the off-by-one wasn't actually fixed:\n{src}");
    }

    /// The milestone test: tursi's own loop does a real agentic task against
    /// DeepSeek — schemas, dispatch, fs tools, sandbox verify, ledger.
    /// `set -a; source .env; set +a; cargo test end_to_end -- --ignored --nocapture`
    #[tokio::test]
    #[ignore = "live: needs DEEPSEEK_API_KEY and network"]
    async fn end_to_end_task_against_deepseek() {
        let key = std::env::var("DEEPSEEK_API_KEY").expect("DEEPSEEK_API_KEY not set");
        let dir = testutil::tmp("e2e");
        std::fs::write(dir.join("notes.txt"), "zebra\n").unwrap();

        let (tb, ui, mut rx) = testutil::toolbox(&dir);
        // Drain UI events so the channel never backpressures the loop.
        tokio::spawn(async move { while rx.recv().await.is_some() {} });

        let mut config = Config::default();
        config.model = "deepseek/deepseek-chat".into();
        config.verify = vec!["grep -q zebra greeting.txt".into()];
        config.max_turns_per_task = 8;

        let prefix = prompt::prefix(
            &test_card(),
            &prompt::ProjectBlock {
                root: dir.display().to_string(),
                languages: vec![],
                verify: config.verify.clone(),
                headless: true,
            },
        );
        let (_steer_tx, steer_rx) = mpsc::channel(4);
        let (_cancel_tx, cancel_rx) = watch::channel(false);
        let ledger = Ledger::open_at(dir.join("stats")).unwrap();
        let session = Uuid::now_v7();
        let mut agent = AgentLoop::new(
            ROOT,
            session,
            prefix,
            config,
            Executor { toolbox: tb },
            ledger,
            ui,
            steer_rx,
            cancel_rx,
        );
        agent.afk = true;

        let provider = Provider::OpenAiCompat(openai_compat::Client::new(
            key,
            "https://api.deepseek.com/v1".to_string(),
        ));
        let outcome = agent
            .run_task(
                &provider,
                "Read notes.txt, then create greeting.txt containing exactly the word you \
                 found, using the write tool. Keep it minimal."
                    .to_string(),
            )
            .await
            .unwrap();

        assert!(
            matches!(outcome, TaskOutcome::Done { .. }),
            "expected Done (verify-gated), transcript had {} messages",
            agent.transcript.len()
        );
        let written = std::fs::read_to_string(dir.join("greeting.txt")).unwrap();
        assert!(written.to_lowercase().contains("zebra"), "got: {written}");
        // The ledger recorded real usage for this session.
        assert!(agent.ledger.session_total(session).is_ok());
    }
}
