//! The core loop (§5.2). `AgentLoop` is a composable unit per §5.7:
//! everything it needs arrives through the constructor — no process-global
//! state — so spawning a subagent later is calling the constructor twice.

pub mod goal;
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
    let changes = Arc::new(Mutex::new(crate::changes::Changes::open(&project)));
    let decider = crate::decide::Decider::from_config(&config, &secrets).map(Arc::new);
    // The station catalog marks which models are on the provider's paid tier
    // (rate limits, §7.2) and prices the DEALS stations (§5.8).
    let mut config = config;
    let catalog = crate::deals::catalog::Catalog::load();
    crate::ratelimit::configure(&config.limits, catalog.stations.iter().filter(|s| s.paid).map(|s| s.model.clone()));
    let stations = if config.deals.enabled { catalog.pool(&config) } else { Vec::new() };
    for s in &stations {
        config.prices.entry(s.model.clone()).or_insert_with(|| s.price());
        config.models.entry(s.model.clone()).or_insert_with(|| s.default_options());
    }
    let spawner = Arc::new(Spawner {
        project: project.clone(),
        config: config.clone(),
        secrets: secrets.clone(),
        prefix: prefix.clone(),
        session: session_id,
        sandbox: sandbox.clone(),
        changes: changes.clone(),
        cancel: cancel_rx.clone(),
        next_id: Arc::new(std::sync::atomic::AtomicU32::new(1)),
        decider: decider.clone(),
        writes: Mutex::new(std::collections::HashMap::new()),
        running: Mutex::new(std::collections::HashSet::new()),
        board: Mutex::new(std::collections::BTreeMap::new()),
        last_look: Mutex::new(None),
        pool: std::sync::OnceLock::new(),
        ui: Mutex::new(None),
    });
    if config.deals.enabled {
        if stations.is_empty() {
            tracing::warn!("[deals] enabled but no qualified stations — run `tursi --stations probe`");
        } else {
            use crate::deals::{expertise::Expertise, memory, pool};
            let weak = Arc::downgrade(&spawner);
            let exec: pool::Exec = Arc::new(move |seg| {
                let weak = weak.clone();
                Box::pin(async move {
                    match weak.upgrade() {
                        Some(spawner) => spawner.run_segment(seg).await,
                        None => pool::SegmentResult { end: pool::SegmentEnd::Interrupted("session ended".into()), cost: 0.0, secs: 0, transcript: vec![], files: vec![] },
                    }
                })
            });
            tracing::info!(stations = stations.len(), "deals: pool ready");
            let pool = pool::Pool::new(
                config.deals.clone(),
                stations,
                Expertise::load(Expertise::default_path()),
                Some(memory::Memory::new(&project, config.deals.memory_cap)),
                memory::Embedder::from_config(&config, &secrets),
                decider.clone(),
                exec,
                project.display().to_string(),
                spawner.next_id.clone(),
            );
            let _ = spawner.pool.set(Arc::new(pool));
        }
    }
    let toolbox = Toolbox {
        agent: ROOT,
        project: project.clone(),
        fs: fs::State::default(),
        lsp: crate::lsp::Manager::new(project.clone(), config.lsp.clone(), sandbox.clone()),
        sandbox: sandbox.clone(),
        debugger: None,
        rizin: None,
        changes,
        custom: custom::Registry::load()?,
        mask: None,
        writes: None,
        denied: if config.solo { SOLO_DENIED } else { ROOT_DENIED },
        subagents: Some(spawner.clone()),
        monitors,
        decider,
        afk,
        lsp_check_edits: config.lsp_check_edits,
        task: 0,
        turn: 0,
        split: None,
        fork: None,
    };
    // The pipeline (§5.7): with the pool up, user messages go straight to it.
    let pipeline = config.deals.pipeline && !config.solo && spawner.pool.get().is_some();
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
    agent.pipeline = pipeline;
    agent.monitor_rx = Some(monitor_rx);
    agent.balance = crate::balance::Balance::from_config(&agent.config, &secrets);
    agent.refresh_balance(true);
    agent.secrets = Some(secrets);
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
    /// The project ledger: this loop's conversation is appended to it.
    events: Arc<crate::ledger::Ledger>,
    /// Messages already in the ledger; history rewritten in place (compaction,
    /// repair) forces a snapshot instead of appends.
    persisted: usize,
    rewrote: bool,
    persisted_goal: Option<String>,
    /// Provider balance reader (None: no known endpoint).
    balance: Option<Arc<crate::balance::Balance>>,
    last_balance_fetch: Option<std::time::Instant>,
    /// `/goal` (`goal.rs`): evaluated at every task end while set.
    pub goal: Option<goal::Goal>,
    /// Credentials for the goal evaluator's model. None in unit tests.
    secrets: Option<crate::config::Secrets>,
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
    /// Dollars this loop has spent on model calls (a subagent's task cost).
    pub spent_usd: f64,
    /// A pool segment's wall-clock limit (§5.8): past it the task ends as
    /// `TimeLimit` at the next turn.
    pub deadline: Option<std::time::Instant>,
    /// The pipeline (§5.7): user messages go straight to the pool; this
    /// loop never calls a model.
    pub pipeline: bool,
    /// The pool agent the conversation is with: the next message follows up
    /// with it, its context intact.
    pipeline_agent: Option<u32>,
}

/// The root agent, when it works (`[deals] pipeline = false`, or no pool):
/// every tool, but it never splits or forks; it files tasks with `agent`.
const ROOT_DENIED: &[&str] = &["split", "fork"];
/// `--station`: one model, alone, measured as itself.
const SOLO_DENIED: &[&str] = &["split", "fork", "agent"];

/// A subagent's permissions (§5.7) follow from the areas its brief declares
/// it writes, nothing else. With none it is a reader: every tool but `edit`
/// and `write`, and its commands run with the project mounted read-only
/// (build directories stay writable), so it can build, test and inspect but
/// not change the project. With areas it is a writer: every tool, and other
/// agents are warned off its areas (`.` is the whole project and claims
/// nothing). The task's labels never change what it may do.
const READER_TOOLS: &[&str] =
    &["read", "search", "execute_command", "profile", "debug", "rizin", "code_intel", "log_search", "decide", "split"];
const READER_TOOLS_FORK: &[&str] =
    &["read", "search", "execute_command", "profile", "debug", "rizin", "code_intel", "log_search", "decide", "split", "fork"];
const WRITER_TOOLS: &[&str] =
    &["read", "write", "edit", "search", "execute_command", "profile", "debug", "rizin", "code_intel", "log_search", "decide", "split"];
const WRITER_TOOLS_FORK: &[&str] =
    &["read", "write", "edit", "search", "execute_command", "profile", "debug", "rizin", "code_intel", "log_search", "decide", "split", "fork"];

/// Appended to a pool subagent's prompt when it may fork (§5.8).
const FORK_ADDENDUM: &str = "## Parallel work\nIf your brief, or what remains of it, is several pieces that need nothing \
    from each other (separate files or modules, lookups that don't depend on each other, on a new project often \
    research, target setup and scaffolding), `fork` them: each runs at the same time on its own subagent, and a \
    fresh one continues with their reports and your `then`. Fork early, before doing the pieces yourself. Keep \
    dependent work in order (`split`, or do it). A task that changes files lists the files or directories it \
    writes, inside your own, and no two may overlap; put shared files (interfaces, configs, the build file) in \
    one task or do them first.\n";

/// `reader` or `writer`: the UI's color and the report header.
pub fn access(writes: &[String]) -> &'static str {
    if writes.is_empty() { "reader" } else { "writer" }
}

fn tools(writes: &[String], can_fork: bool) -> &'static [&'static str] {
    match (writes.is_empty(), can_fork) {
        (true, false) => READER_TOOLS,
        (true, true) => READER_TOOLS_FORK,
        (false, false) => WRITER_TOOLS,
        (false, true) => WRITER_TOOLS_FORK,
    }
}

/// Appended to a subagent's prefix: what it may change and how to report.
/// Research and review get their report shapes; everything else reports
/// what changed and what ran.
fn addendum(writes: &[String], activity: Option<crate::deals::Activity>, from_user: bool) -> String {
    use crate::deals::Activity;
    if from_user {
        return "## The request\nThe message is the user's own request, and your final message is your reply to them: what you \
            did and the evidence (core rule 3), or the answer they asked for, in a few lines. You can change files anywhere \
            in the project. Work silently between tool calls. If something is ambiguous, take the reasonable reading and say \
            what you assumed. If the request turns out to be two jobs in order, or your context is getting long, `split` \
            hands back what you finished and what remains, and a fresh agent continues."
            .to_string();
    }
    let access = if writes.is_empty() {
        "## Subagent (read-only)\nYou work on the brief in this project without changing it: the project is mounted \
         read-only, so you can read, search, build, test and run things (build directories are writable), and no \
         project file can change. If the brief needs a change, say exactly what in your report."
            .to_string()
    } else if writes.iter().any(|w| w == ".") {
        "## Subagent\nYou do the brief, completely and nothing more, in this project. Run what you change.".to_string()
    } else {
        format!(
            "## Subagent\nYou do the brief, completely and nothing more, in this project. Change files only in: {}; \
             other agents may be working elsewhere in the project at the same time. Run what you change.",
            writes.join(", ")
        )
    };
    let report = match activity {
        Some(Activity::Review) => "Judge what the brief describes against its intent or the rules it names: \
            correctness first, then missed cases, then style. Your final message is the whole report the parent \
            agent gets: findings ordered by severity, one or two lines each with file:line and why it matters, any \
            file you wrote; say plainly if it looks right.",
        _ if writes.is_empty() => "Your final message is the whole report the parent agent gets. Answer the brief \
            directly: the findings, each with file:line and the decisive line quoted when it matters, then what you \
            could not determine. No preamble, no restating the brief, no headings for a short answer; as short as the \
            answer allows.",
        _ => "Your final message is the whole report the parent agent gets, in a few lines: what you changed \
            (files), what you ran and the result, and anything the brief left open. No preamble and no recap of the \
            brief.",
    };
    format!(
        "{access}\nWork silently: call tools with no status lines or commentary between them — your calls are shown as \
         they happen and nobody reads narration. {report} You have no subagents; the delegation rule is for your \
         parent. If the brief turns out to be two jobs, or your context is getting long, `split` hands back what you \
         finished and what remains, and a fresh subagent continues."
    )
}

/// What a subagent needs that its parent has: config, credentials, the
/// prefix, the sandbox, the shared change recorder, the parent's cancel.
pub struct Spawner {
    project: PathBuf,
    config: Config,
    secrets: crate::config::Secrets,
    prefix: String,
    session: Uuid,
    sandbox: crate::sandbox::Sandbox,
    changes: Arc<Mutex<crate::changes::Changes>>,
    cancel: watch::Receiver<bool>,
    /// agent-N ids; shared with the DEALS pool, whose fork subtasks get ids too.
    next_id: Arc<std::sync::atomic::AtomicU32>,
    decider: Option<Arc<crate::decide::Decider>>,
    /// Write areas of every child spawned this session: a follow-up that
    /// declares none keeps the child's.
    writes: Mutex<std::collections::HashMap<u32, Vec<String>>>,
    /// Children still working: a follow-up must wait for their report.
    running: Mutex<std::collections::HashSet<u32>>,
    /// Every task the lead filed this session, for its view of the
    /// pipeline (`tasks`); live state comes from the pool.
    board: Mutex<std::collections::BTreeMap<u32, Filed>>,
    /// The board as `tasks` last showed it, without times: asked again
    /// unchanged, `tasks` waits for a change.
    last_look: Mutex<Option<String>>,
    /// DEALS (§5.8): when set, tasks queue at model stations instead of
    /// starting on the subagent model.
    pool: std::sync::OnceLock<Arc<crate::deals::pool::Pool>>,
    /// The lead's UI handle, for segments the pool starts later.
    ui: Mutex<Option<UiHandle>>,
}

/// Longest report handed back; the child's full transcript is persisted.
const REPORT_CAP: usize = 6000;
/// How far past its time limit a segment may run before it is stopped from
/// outside (§5.8): room for the turn in flight to finish on its own.
const HARD_LIMIT_GRACE: Duration = Duration::from_secs(120);

/// A filed task on the lead's board.
struct Filed {
    kind: String,
    title: String,
    writes: Vec<String>,
    started: std::time::Instant,
    done: Option<Done>,
}

struct Done {
    model: String,
    secs: u64,
    /// The QA judge's figure (DEALS only).
    qa: Option<f64>,
    ok: bool,
}

impl Spawner {
    /// Run one subagent to completion and return its report. The child's
    /// events reach the UI under its own agent id (collapsed by default).
    #[allow(dead_code)]
    pub async fn run(&self, writes: Vec<String>, brief: String, parent_ui: &UiHandle) -> Result<String> {
        let n = self.next_id.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        self.writes.lock().unwrap().insert(n, writes.clone());
        self.spawn(n, writes, brief, parent_ui, None).await
    }

    /// Start a child in the background: returns at once; the report arrives
    /// as a `Report` event through `monitors` when the child finishes. Several
    /// children run concurrently. With DEALS on, the task is labelled, queues
    /// at a station, and the pool decides which model runs it.
    pub async fn start(
        self: &Arc<Self>,
        writes: Vec<String>,
        needs: Vec<String>,
        brief: String,
        parent_ui: &UiHandle,
        monitors: &mut crate::monitor::Manager,
    ) -> Result<String> {
        *self.ui.lock().unwrap() = Some(parent_ui.clone());
        let n = self.next_id.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        if let Some(pool) = self.pool.get().cloned() {
            // What kind of work it is decides which station is likely to succeed (§5.8).
            let labels = crate::deals::labels::label(self.decider.as_deref(), &brief).await;
            let label = Self::label(&labels.name(), &brief);
            self.file(n, labels.name(), &brief, &writes);
            let (receipt, rx) = pool.submit(n, labels, brief, needs, writes.clone(), self.config.subagent.model.as_deref(), false)?;
            self.writes.lock().unwrap().insert(n, writes);
            return Ok(self.launch_pooled(n, labels.name(), label, receipt, rx, monitors));
        }
        self.file(n, access(&writes).to_string(), &brief, &writes);
        self.writes.lock().unwrap().insert(n, writes.clone());
        Ok(self.launch(n, writes, brief, parent_ui, monitors, None))
    }

    /// Background follow-up with an earlier child (see `continue_agent`).
    /// `writes` replaces the child's areas; None keeps them.
    pub async fn continue_in_background(
        self: &Arc<Self>,
        n: u32,
        brief: String,
        writes: Option<Vec<String>>,
        parent_ui: &UiHandle,
        monitors: &mut crate::monitor::Manager,
    ) -> Result<String> {
        *self.ui.lock().unwrap() = Some(parent_ui.clone());
        let writes = match writes {
            Some(w) => w,
            None => self
                .writes
                .lock()
                .unwrap()
                .get(&n)
                .cloned()
                .ok_or_else(|| anyhow::anyhow!("no agent-{n} in this session — start one with a brief"))?,
        };
        if self.running.lock().unwrap().contains(&n) {
            anyhow::bail!("agent-{n} is still working — its report will arrive when it finishes; follow up after that");
        }
        let conv = self.load_child(n)?;
        let (messages, task) = (conv.messages, conv.task);
        self.writes.lock().unwrap().insert(n, writes.clone());
        if let Some(pool) = self.pool.get().cloned() {
            let labels = crate::deals::labels::label(self.decider.as_deref(), &brief).await;
            let label = Self::label(&labels.name(), &brief);
            self.file(n, labels.name(), &brief, &writes);
            let (receipt, rx) = pool.follow_up(n, labels, brief, writes, (messages, task), false)?;
            return Ok(self.launch_pooled(n, labels.name(), label, receipt, rx, monitors));
        }
        self.file(n, access(&writes).to_string(), &brief, &writes);
        Ok(self.launch(n, writes, brief, parent_ui, monitors, Some((messages, task))))
    }

    fn file(&self, n: u32, kind: String, brief: &str, writes: &[String]) {
        let filed = Filed { kind, title: Self::title(brief).chars().take(80).collect(), writes: writes.to_vec(), started: std::time::Instant::now(), done: None };
        self.board.lock().unwrap().insert(n, filed);
    }

    /// The pipeline's half (§5.7): the user's message as a pool task that
    /// may write anywhere, labelled like any task, waited for. `follow`
    /// continues that agent with its context; when it can't be (it never ran
    /// in this process), a fresh one starts.
    pub async fn run_pipeline(
        self: &Arc<Self>,
        follow: Option<u32>,
        brief: String,
        parent_ui: &UiHandle,
    ) -> Result<(u32, crate::deals::pool::Finished)> {
        *self.ui.lock().unwrap() = Some(parent_ui.clone());
        let pool = self.pool.get().cloned().ok_or_else(|| anyhow::anyhow!("the pipeline needs the DEALS pool"))?;
        let labels = crate::deals::labels::label(self.decider.as_deref(), &brief).await;
        let writes = vec![".".to_string()];
        let resumed = follow.and_then(|n| self.load_child(n).ok().map(|c| (n, c)));
        let (n, rx) = match resumed.and_then(|(n, c)| pool.follow_up(n, labels, brief.clone(), writes.clone(), (c.messages, c.task), true).ok().map(|(_, rx)| (n, rx))) {
            Some(found) => found,
            None => {
                let n = self.next_id.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                let (_, rx) = pool.submit(n, labels, brief.clone(), vec![], writes.clone(), self.config.subagent.model.as_deref(), true)?;
                (n, rx)
            }
        };
        self.file(n, labels.name(), &brief, &writes);
        self.writes.lock().unwrap().insert(n, writes);
        self.running.lock().unwrap().insert(n);
        let finished = rx.await.map_err(|_| anyhow::anyhow!("the pool dropped the task"));
        self.running.lock().unwrap().remove(&n);
        let finished = finished?;
        self.finished(n, Done { model: finished.model.clone(), secs: finished.secs, qa: Some(finished.qa), ok: finished.ok });
        Ok((n, finished))
    }

    fn finished(&self, n: u32, done: Done) {
        if let Some(f) = self.board.lock().unwrap().get_mut(&n) {
            f.done = Some(done);
        }
    }

    /// `tasks`: the board. Asked again while nothing has changed and tasks
    /// are still out, it waits up to a minute for the next change instead of
    /// answering at once, so a lead that polls costs no model calls.
    pub async fn look(&self) -> String {
        let sig = self.board(false);
        let again = self.last_look.lock().unwrap().replace(sig.clone()).as_deref() == Some(sig.as_str());
        let outstanding = self.board.lock().unwrap().values().any(|f| f.done.is_none());
        if again && outstanding {
            let started = std::time::Instant::now();
            while started.elapsed() < std::time::Duration::from_secs(60) {
                tokio::time::sleep(std::time::Duration::from_secs(1)).await;
                let now = self.board(false);
                if now != sig {
                    *self.last_look.lock().unwrap() = Some(now);
                    return format!("(changed after {}s)\n{}", started.elapsed().as_secs(), self.board(true));
                }
            }
            return format!(
                "(no change in {}s; don't poll: end your turn, and each report wakes you)\n{}",
                started.elapsed().as_secs(),
                self.board(true)
            );
        }
        self.board(true)
    }

    /// The lead's view of the pipeline: one line per filed task, a fork's
    /// subtasks under it, and the pool's load. Without `times`, no elapsed
    /// seconds: the board's state alone, for spotting a change.
    fn board(&self, times: bool) -> String {
        use crate::deals::pool::LiveState;
        use crate::deals::short_model;
        let live = self.pool.get().map(|p| p.snapshot()).unwrap_or_default();
        let area = |w: &[String]| if w.is_empty() { "read-only".to_string() } else { format!("writes {}", w.join(", ")) };
        let state = |l: &crate::deals::pool::Live| match &l.state {
            LiveState::Queued(0) => format!("queued at {}, next", short_model(&l.model)),
            LiveState::Queued(n) => format!("queued at {}, {n} ahead", short_model(&l.model)),
            LiveState::Running if times => format!("running on {} for {}s", short_model(&l.model), l.secs),
            LiveState::Running => format!("running on {}", short_model(&l.model)),
            LiveState::Waiting(ids) => {
                format!("forked, waiting for {}", ids.iter().map(|i| format!("agent-{i}")).collect::<Vec<_>>().join(", "))
            }
        };
        let board = self.board.lock().unwrap();
        let mut out = Vec::new();
        for (n, f) in board.iter() {
            let now = match (&f.done, live.iter().find(|l| l.id == *n)) {
                (Some(d), _) => {
                    let qa = d.qa.map(|q| format!(", QA {q:.2}")).unwrap_or_default();
                    let on = if d.model.is_empty() { String::new() } else { format!(" on {}", short_model(&d.model)) };
                    format!("{}{on}, {}s{qa}", if d.ok { "done" } else { "unfinished" }, d.secs)
                }
                (None, Some(l)) => {
                    let splits = if l.splits > 0 { format!(", {} split{}", l.splits, if l.splits == 1 { "" } else { "s" }) } else { String::new() };
                    format!("{}{splits}", state(l))
                }
                (None, None) if times => format!("running for {}s", f.started.elapsed().as_secs()),
                (None, None) => "running".to_string(),
            };
            out.push(format!("agent-{n}  {}  {now}  {} — {}", f.kind, area(&f.writes), f.title));
            for sub in live.iter().filter(|l| l.parent == Some(*n)) {
                out.push(format!("  agent-{}  {}  {}  {} — {}", sub.id, sub.labels.name(), state(sub), area(&sub.writes), sub.title));
            }
        }
        if out.is_empty() {
            return "No tasks filed yet.".into();
        }
        if let Some(pool) = self.pool.get() {
            let (running, queued) = pool.load();
            out.push(format!("pool: {running} running, {queued} queued"));
        }
        out.join("\n")
    }

    fn title(brief: &str) -> String {
        brief.lines().map(str::trim).find(|l| !l.is_empty()).unwrap_or("").chars().take(120).collect()
    }

    /// `<kind> <title>`: the job's label, and the head of its wake-up line.
    fn label(kind: &str, brief: &str) -> String {
        format!("{kind} {}", Self::title(brief).chars().take(50).collect::<String>())
    }

    /// A pooled task: the job waits for the pool's verdict and formats the
    /// report; the lead hears where the task landed.
    fn launch_pooled(
        self: &Arc<Self>,
        n: u32,
        kind: String,
        label: String,
        receipt: crate::deals::pool::Receipt,
        rx: tokio::sync::oneshot::Receiver<crate::deals::pool::Finished>,
        monitors: &mut crate::monitor::Manager,
    ) -> String {
        self.running.lock().unwrap().insert(n);
        let me = self.clone();
        let head = kind.clone();
        monitors.spawn_job(format!("agent-{n}"), label, "agent", async move {
            let out = match rx.await {
                Ok(f) => {
                    let mut report = f.report;
                    if report.len() > REPORT_CAP {
                        let cut = report.char_indices().nth(REPORT_CAP).map(|(i, _)| i).unwrap_or(report.len());
                        report.truncate(cut);
                        report.push_str("\n… (report truncated)");
                    }
                    let mut route = String::new();
                    if f.splits > 0 {
                        route.push_str(&format!(", {} split{}", f.splits, if f.splits == 1 { "" } else { "s" }));
                    }
                    me.finished(n, Done { model: f.model.clone(), secs: f.secs, qa: Some(f.qa), ok: f.ok });
                    format!(
                        "[{head} agent-{n} on {}, {}s, QA {:.2}{route}{} — follow up with agent:\"agent-{n}\"]\n{report}",
                        crate::deals::short_model(&f.model),
                        f.secs,
                        f.qa,
                        if f.ok { "" } else { ", unfinished" }
                    )
                }
                Err(_) => {
                    me.finished(n, Done { model: String::new(), secs: 0, qa: None, ok: false });
                    format!("[{head} agent-{n} failed] the pool dropped this task")
                }
            };
            me.running.lock().unwrap().remove(&n);
            out
        });
        let model = crate::deals::short_model(&receipt.model);
        match receipt.ahead {
            None => format!(
                "[{kind} agent-{n} started on {model} — its report arrives as a message when it finishes. Launch every other \
                 independent piece now; end your turn when nothing else can start until reports arrive.]"
            ),
            Some(ahead) => format!(
                "[{kind} agent-{n} queued at {model} ({ahead} ahead) — it starts when a slot frees and its report arrives as \
                 a message. Keep filing independent pieces; end your turn when nothing else can start.]"
            ),
        }
    }

    fn launch(
        self: &Arc<Self>,
        n: u32,
        writes: Vec<String>,
        brief: String,
        parent_ui: &UiHandle,
        monitors: &mut crate::monitor::Manager,
        resume: Option<(Vec<Message>, u32)>,
    ) -> String {
        self.running.lock().unwrap().insert(n);
        let kind = access(&writes);
        let title = Self::title(&brief);
        let label = Self::label(kind, &brief);
        let child_ui = UiHandle { agent: AgentId(n), tx: parent_ui.tx.clone() };
        let continued = resume.is_some();
        let model = self.config.subagent.model.clone().unwrap_or_else(|| self.config.model.clone());
        let (me, ui, brief2) = (self.clone(), parent_ui.clone(), brief);
        monitors.spawn_job(format!("agent-{n}"), label, "agent", async move {
            let started = std::time::Instant::now();
            child_ui.send(EventKind::SubagentStarted { access: kind.to_string(), title, model: model.clone(), continued }).await;
            let (out, ok) = match me.spawn(n, writes, brief2, &ui, resume).await {
                Ok(report) => {
                    let ok = !["(interrupted)", "(turn limit", "(budget cap"].iter().any(|m| report.contains(m));
                    (report, ok)
                }
                Err(e) => (format!("[{kind} agent-{n} failed] {e:#}"), false),
            };
            child_ui.send(EventKind::SubagentFinished { ok }).await;
            me.finished(n, Done { model, secs: started.elapsed().as_secs(), qa: None, ok });
            me.running.lock().unwrap().remove(&n);
            out
        });
        format!(
            "[{kind} agent-{n} started — its report arrives as a message when it finishes. Keep working on other \
             things, or end your turn to wait for it.]"
        )
    }

    /// The pool's executor: one segment of a task on the station's model.
    async fn run_segment(self: Arc<Self>, seg: crate::deals::pool::Segment) -> crate::deals::pool::SegmentResult {
        use crate::deals::pool::{SegmentEnd, SegmentResult};
        let Some(ui) = self.ui.lock().unwrap().clone() else {
            return SegmentResult { end: SegmentEnd::Failed("no session UI".into()), cost: 0.0, secs: 0, transcript: vec![], files: vec![] };
        };
        let child_ui = UiHandle { agent: AgentId(seg.task), tx: ui.tx.clone() };
        child_ui
            .send(EventKind::SubagentStarted { access: access(&seg.writes).to_string(), title: seg.title.clone(), model: seg.model.clone(), continued: seg.continued })
            .await;
        // A writer announces the areas it writes; others are warned off them.
        // `.` (the whole project) claims nothing.
        let areas: Vec<PathBuf> = seg.writes.iter().filter(|w| *w != ".").map(|w| self.project.join(w)).collect();
        if !areas.is_empty() {
            self.changes.lock().unwrap().set_intent(AgentId(seg.task), areas.clone());
        }
        let run = self
            .run_child(seg.task, &seg.writes, seg.labels.activity, seg.from_user, seg.brief, &ui, seg.resume, Some(seg.model.clone()), seg.can_fork, seg.time_limit)
            .await;
        if !areas.is_empty() {
            self.changes.lock().unwrap().clear_intent(AgentId(seg.task));
        }
        let result = match run {
            Err(e) => SegmentResult { end: SegmentEnd::Failed(format!("{e:#}")), cost: 0.0, secs: 0, transcript: vec![], files: vec![] },
            Ok(run) => {
                let end = match run.outcome {
                    Ok(TaskOutcome::Done { summary }) | Ok(TaskOutcome::NeedsUser { text: summary }) => {
                        SegmentEnd::Done(if summary.trim().is_empty() { run.last_text } else { summary })
                    }
                    Ok(TaskOutcome::Split { done, remaining }) => SegmentEnd::Split { done, remaining },
                    Ok(TaskOutcome::Fork { done, tasks, then }) => SegmentEnd::Fork { done, children: tasks, then },
                    Ok(TaskOutcome::TurnLimit) => SegmentEnd::TurnLimit(run.last_text),
                    Ok(TaskOutcome::TimeLimit) => SegmentEnd::TurnLimit(format!("(ran out of time)\n{}", run.last_text)),
                    Ok(TaskOutcome::Interrupted) => SegmentEnd::Interrupted(run.last_text),
                    Ok(TaskOutcome::BudgetHalt) => SegmentEnd::Interrupted(format!("(budget cap reached)\n{}", run.last_text)),
                    Err(e) => SegmentEnd::Failed(format!("{e:#}")),
                };
                SegmentResult { end, cost: run.cost, secs: run.secs, transcript: run.transcript, files: vec![] }
            }
        };
        // Every file this segment changed, shell-made ones included (QA evidence).
        let mut result = result;
        result.files = self
            .changes
            .lock()
            .unwrap()
            .take_changed(AgentId(seg.task))
            .into_iter()
            .map(|p| p.strip_prefix(&self.project).unwrap_or(&p).display().to_string())
            .collect();
        let ok = matches!(result.end, SegmentEnd::Done(_) | SegmentEnd::Split { .. } | SegmentEnd::Fork { .. });
        child_ui.send(EventKind::SubagentFinished { ok }).await;
        result
    }

    fn load_child(&self, n: u32) -> Result<crate::ledger::Conversation> {
        crate::ledger::for_project(&self.project)
            .conversation(self.session, AgentId(n))
            .ok_or_else(|| anyhow::anyhow!("agent-{n} left no conversation to continue from"))
    }

    /// Follow up with an earlier child: same write areas, its transcript
    /// reloaded from disk, the new brief as its next task. Cheap on
    /// cache-friendly models — the child's whole context is a stable prefix.
    #[allow(dead_code)]
    pub async fn continue_agent(&self, n: u32, brief: String, parent_ui: &UiHandle) -> Result<String> {
        let writes = self
            .writes
            .lock()
            .unwrap()
            .get(&n)
            .cloned()
            .ok_or_else(|| anyhow::anyhow!("no agent-{n} in this session — start one with a brief"))?;
        if self.running.lock().unwrap().contains(&n) {
            anyhow::bail!("agent-{n} is still working — its report will arrive when it finishes; follow up after that");
        }
        let conv = self.load_child(n)?;
        self.spawn(n, writes, brief, parent_ui, Some((conv.messages, conv.task))).await
    }

    /// Run a child to its outcome. `model` overrides the subagent model (a
    /// DEALS station); without it `[subagent] model` runs.
    #[allow(clippy::too_many_arguments)]
    async fn run_child(
        &self,
        n: u32,
        writes: &[String],
        activity: Option<crate::deals::Activity>,
        from_user: bool,
        brief: String,
        parent_ui: &UiHandle,
        resume: Option<(Vec<Message>, u32)>,
        model: Option<String>,
        can_fork: bool,
        time_limit: Option<std::time::Duration>,
    ) -> Result<ChildRun> {
        let id = AgentId(n);
        let settings = self.config.subagent.clone();
        let mut config = self.config.clone();
        if let Some(model) = model.or(settings.model) {
            config.model = model;
            config.fallbacks.clear();
        }
        config.max_turns_per_task = settings.max_turns.unwrap_or(30);
        config.verify.clear(); // the parent owns verification
        let provider = Provider::for_model(&config.model, &self.secrets, &config)?;

        // A reader's commands, background ones included, see the project read-only.
        let sandbox = if writes.is_empty() { self.sandbox.with_read_only_project() } else { self.sandbox.clone() };
        let (monitors, _monitor_rx) = crate::monitor::Manager::new(sandbox.clone());
        let monitors = monitors.for_agent(id);
        let toolbox = Toolbox {
            agent: id,
            project: self.project.clone(),
            fs: fs::State::default(),
            lsp: crate::lsp::Manager::new(self.project.clone(), config.lsp.clone(), sandbox.clone()),
            sandbox,
            debugger: None,
            rizin: None,
            changes: self.changes.clone(),
            custom: custom::Registry { entries: vec![] },
            mask: Some(tools(writes, can_fork)),
            denied: &[],
            writes: Some(writes.to_vec()),
            subagents: None,
            monitors,
            decider: self.decider.clone(),
            afk: true,
            lsp_check_edits: config.lsp_check_edits,
            task: 0,
            turn: 0,
            split: None,
            fork: None,
        };
        let (_steer_tx, steer_rx) = mpsc::channel(1);
        let ui = UiHandle { agent: id, tx: parent_ui.tx.clone() };
        let mut child = AgentLoop::new(
            id,
            self.session,
            format!("{}\n{}\n{}", self.prefix, addendum(writes, activity, from_user), if can_fork { FORK_ADDENDUM } else { "" }),
            config,
            Executor { toolbox },
            Ledger::open()?,
            ui,
            steer_rx,
            self.cancel.clone(),
        );
        child.afk = true;
        child.deadline = time_limit.map(|t| std::time::Instant::now() + t);
        let continued = resume.is_some();
        if let Some((messages, task)) = resume {
            child.adopt(crate::ledger::Conversation { task, goal: None, messages });
        }
        let started = std::time::Instant::now();
        // The loop recurses through the tool call: box the child's future.
        // The deadline is checked between turns; this outer bound (a grace
        // past it) ends the segment whatever it is stuck in, so no hang can
        // hold a task forever.
        let run = Box::pin(child.run_task(&provider, brief));
        let outcome = match time_limit {
            Some(limit) => match tokio::time::timeout(limit + HARD_LIMIT_GRACE, run).await {
                Ok(outcome) => outcome,
                Err(_) => {
                    tracing::warn!(agent = n, model = %child.config.model, secs = started.elapsed().as_secs(), "segment stuck past its time limit: stopped");
                    child.repair_transcript("stopped: the segment ran past its time limit");
                    Ok(TaskOutcome::TimeLimit)
                }
            },
            None => run.await,
        };
        child.persist();
        let last_text = child
            .transcript
            .iter()
            .rev()
            .find_map(|m| match m {
                Message::Assistant { text, .. } if !text.trim().is_empty() => Some(text.trim().to_string()),
                _ => None,
            })
            .unwrap_or_default();
        let secs = started.elapsed().as_secs();
        tracing::info!(agent = n, access = access(writes), model = %child.config.model, continued, secs, cost = child.spent_usd, "subagent finished");
        Ok(ChildRun { outcome, last_text, cost: child.spent_usd, secs, continued, transcript: std::mem::take(&mut child.transcript) })
    }

    async fn spawn(
        &self,
        n: u32,
        writes: Vec<String>,
        brief: String,
        parent_ui: &UiHandle,
        resume: Option<(Vec<Message>, u32)>,
    ) -> Result<String> {
        let run = self.run_child(n, &writes, None, false, brief, parent_ui, resume, None, false, None).await?;
        let last_text = run.last_text;
        let mut report = match run.outcome? {
            TaskOutcome::Done { summary } | TaskOutcome::NeedsUser { text: summary } => {
                if summary.trim().is_empty() { last_text } else { summary }
            }
            TaskOutcome::Interrupted => format!("(interrupted)\n{last_text}"),
            TaskOutcome::TurnLimit => format!("(turn limit reached before the brief was finished; last state:)\n{last_text}"),
            TaskOutcome::TimeLimit => format!("(time limit reached before the brief was finished; last state:)\n{last_text}"),
            TaskOutcome::BudgetHalt => format!("(budget cap reached)\n{last_text}"),
            TaskOutcome::Split { done, remaining } => format!("(handed back part of the brief)\n{done}\n\nNot finished: {remaining}"),
            TaskOutcome::Fork { done, tasks, then } => format!(
                "(asked to fork into {} parallel tasks — forking needs DEALS)\n{done}\n\nNot finished: {}\nThen: {then}",
                tasks.len(),
                tasks.iter().map(|t| t.brief.lines().next().unwrap_or("").to_string()).collect::<Vec<_>>().join("; ")
            ),
        };
        if report.len() > REPORT_CAP {
            let cut = report.char_indices().nth(REPORT_CAP).map(|(i, _)| i).unwrap_or(report.len());
            report.truncate(cut);
            report.push_str("\n… (report truncated)");
        }
        Ok(format!(
            "[{} agent-{n}{}, {}s — follow up with agent:\"agent-{n}\"]\n{report}",
            access(&writes),
            if run.continued { " continued" } else { "" },
            run.secs
        ))
    }
}

/// The last few exchanges of a conversation, for a pipeline agent that
/// joins it fresh. None when there are none.
fn recent_context(transcript: &[Message]) -> Option<String> {
    let turns: Vec<String> = transcript
        .iter()
        .filter_map(|m| match m {
            Message::User(t) => Some(format!("user: {}", t.chars().take(800).collect::<String>())),
            Message::Assistant { text, .. } if !text.trim().is_empty() => Some(format!("tursi: {}", text.chars().take(800).collect::<String>())),
            _ => None,
        })
        .collect();
    if turns.is_empty() {
        return None;
    }
    let last: Vec<String> = turns[turns.len().saturating_sub(6)..].to_vec();
    Some(format!("[earlier in this conversation, most recent last]\n{}\n[the new message]", last.join("\n")))
}

/// One child run, before it becomes a report (plain) or a segment (DEALS).
struct ChildRun {
    outcome: Result<TaskOutcome>,
    last_text: String,
    transcript: Vec<Message>,
    cost: f64,
    secs: u64,
    continued: bool,
}

pub enum TaskOutcome {
    Done { summary: String },
    /// Attended turn end that isn't a done-claim: report or question (§5.3).
    NeedsUser { text: String },
    Interrupted,
    TurnLimit,
    /// A pool segment ran past its wall-clock limit (§5.8).
    TimeLimit,
    BudgetHalt,
    /// A subagent handed back part of its brief (`split`, §5.8).
    Split { done: String, remaining: String },
    /// A subagent forked independent parts into parallel subtasks (`fork`,
    /// §5.8 — an extension of DEALS).
    Fork { done: String, tasks: Vec<crate::deals::pool::ChildSpec>, then: String },
}

/// The UI's control plane for the agent task; steering and cancel have their
/// own channels (§5.2).
pub enum Command {
    Task(String),
    SetAfk(bool),
    /// `/plan` — next task is a skeleton (§5.5).
    PlanEnter,
    /// `/approve` — run the typecheck gate, start fill-in (§5.5).
    PlanApprove,
    /// `/model` pin (None unpins).
    Pin(Option<String>),
    /// `/monitor stop <id>`.
    MonitorStop(String),
    /// `/goal <condition>`: set (replacing any) and start working.
    GoalSet(String),
    /// `/goal clear`.
    GoalClear,
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
        let events = crate::ledger::for_project(&executor.toolbox.project);
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
            events,
            persisted: 0,
            rewrote: false,
            persisted_goal: None,
            balance: None,
            last_balance_fetch: None,
            goal: None,
            secrets: None,
            ui,
            config,
            edited_this_task: false,
            ran_since_edit: false,
            unverified_edit_turns: 0,
            empty_done_nudges: 0,
            run_nudges: 0,
            spent_usd: 0.0,
            deadline: None,
            pipeline: false,
            pipeline_agent: None,
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
                Command::GoalSet(condition) => {
                    self.goal = Some(goal::Goal::new(condition));
                    self.send_goal().await;
                    let directive = self.goal.as_ref().map(|g| g.directive()).unwrap_or_default();
                    self.run_instruction(&secrets, directive).await;
                }
                Command::GoalClear => {
                    let summary = match self.goal.take() {
                        Some(g) => format!("goal cleared: {}", g.condition),
                        None => "no goal set".to_string(),
                    };
                    self.send_goal().await;
                    self.ui.send(EventKind::TaskDone { summary }).await;
                }
                Command::MonitorStop(id) => {
                    let summary = if self.executor.toolbox.monitors.stop(&id) {
                        format!("monitor {id} stopped")
                    } else {
                        format!("✗ no monitor {id}")
                    };
                    self.send_monitors().await;
                    self.ui.send(EventKind::TaskDone { summary }).await;
                }
            }
        }
        // Command channel closed = UI shut down cleanly.
    }

    /// One user instruction (or monitor wake) end to end, reported as TaskDone.
    async fn run_instruction(&mut self, secrets: &crate::config::Secrets, instruction: String) {
        self.router.reset();
        if self.plan.active {
            self.transcript.push(Message::User(prompt::plan_injection().to_string()));
        }
        let outcome = if self.pipeline {
            self.run_pipeline(instruction).await
        } else {
            match Provider::for_model(self.router.model(), secrets, &self.config) {
                Ok(provider) => self.run_task(&provider, instruction).await,
                Err(e) => {
                    self.ui.send(EventKind::TaskDone { summary: format!("✗ {e:#}") }).await;
                    return;
                }
            }
        };
        // Errors the user has to fix clear the goal; everything else leaves it.
        if matches!(outcome, Ok(TaskOutcome::BudgetHalt) | Err(_)) && self.goal.take().is_some() {
            self.ui.send(EventKind::GoalVerdict { verdict: "cleared".into(), reason: "the task failed on an error you have to fix — run :goal again to continue".into() }).await;
            self.send_goal().await;
        }
        let summary = match outcome {
            Ok(TaskOutcome::Done { summary }) => summary,
            Ok(TaskOutcome::NeedsUser { text }) => text,
            Ok(TaskOutcome::Interrupted) => "⏹ interrupted".to_string(),
            Ok(TaskOutcome::TurnLimit) => "✗ turn limit reached — task incomplete".to_string(),
            Ok(TaskOutcome::TimeLimit) => "✗ time limit reached — task incomplete".to_string(),
            Ok(TaskOutcome::BudgetHalt) => "✗ budget cap reached — task halted".to_string(),
            Ok(TaskOutcome::Split { done, remaining }) => format!("{done}\n\nNot finished: {remaining}"),
            Ok(TaskOutcome::Fork { done, then, .. }) => format!("{done}\n\nNot finished: {then}"),
            Err(e) => format!("✗ task failed: {e:#}"),
        };
        self.persist();
        let session_usd = self.ledger.session_total(self.session).unwrap_or(0.0);
        let month_usd = self.ledger.month_total().unwrap_or(0.0);
        self.ui.send(EventKind::Cost { session_usd, month_usd }).await;
        self.refresh_balance(true);
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
    /// Append what's new in the conversation to the ledger: new messages as
    /// `message` events, or a `snapshot` when history was rewritten.
    fn persist(&mut self) {
        let goal = self.goal.as_ref().map(|g| g.condition.clone());
        if self.rewrote || self.transcript.len() < self.persisted {
            self.events.append_for(
                self.session,
                self.id,
                crate::ledger::What::Snapshot { task: self.task, goal: goal.clone(), messages: self.transcript.clone() },
            );
            self.rewrote = false;
        } else {
            for message in &self.transcript[self.persisted..] {
                self.events.append_for(self.session, self.id, crate::ledger::What::Message { task: self.task, message: message.clone() });
            }
            if goal != self.persisted_goal {
                self.events.append_for(self.session, self.id, crate::ledger::What::Goal { condition: goal.clone() });
            }
        }
        self.persisted = self.transcript.len();
        self.persisted_goal = goal;
    }

    /// Adopt a conversation already in the ledger (resume, follow-up): it is
    /// persisted as it stands, so only what comes next is appended.
    fn adopt(&mut self, conv: crate::ledger::Conversation) {
        self.task = conv.task;
        self.executor.toolbox.task = conv.task;
        self.transcript = conv.messages;
        self.persisted = self.transcript.len();
        self.persisted_goal = conv.goal.clone();
        // An active goal carries over; its counters start again.
        self.goal = conv.goal.map(goal::Goal::new);
    }

    /// Replay a resumed session's conversation from the ledger: unanswered
    /// calls are repaired, and a `[resumed]` note tells the model what did
    /// not survive (monitors, background jobs, its read-state). Returns the
    /// message count.
    pub fn restore(&mut self) -> Result<usize> {
        let Some(conv) = self.events.conversation(self.session, self.id) else { return Ok(0) };
        self.adopt(conv);
        self.repair_transcript("session ended before this call completed");
        if !self.transcript.is_empty() {
            self.transcript.push(Message::User(
                "[resumed] This session was resumed in a new process. Monitors and background jobs \
                 from before are gone; files may have changed since — re-read before editing."
                    .to_string(),
            ));
        }
        Ok(self.transcript.len())
    }

    /// A monitor that exited or timed out is gone from the list.
    fn note_monitor_end(&mut self, event: &crate::monitor::Event) {
        if matches!(event.what, crate::monitor::What::Exited { .. } | crate::monitor::What::TimedOut { .. } | crate::monitor::What::Report { .. }) {
            self.executor.toolbox.monitors.forget(&event.id);
        }
    }

    async fn send_goal(&self) {
        let (condition, turns, last_reason) = match &self.goal {
            Some(g) => (Some(g.condition.clone()), g.turns, g.last_reason.clone()),
            None => (None, 0, None),
        };
        self.ui.send(EventKind::Goal { condition, turns, last_reason }).await;
    }

    /// The goal gate at a task's end (§5.3): evaluate the condition and
    /// either keep the loop going (Ok(None)) or let the task end. Deferred
    /// while monitors or background jobs are running — their wake continues
    /// the goal.
    async fn goal_gate(&mut self, outcome: TaskOutcome) -> Result<Option<TaskOutcome>> {
        let Some(goal) = self.goal.as_mut() else { return Ok(Some(outcome)) };
        if !matches!(outcome, TaskOutcome::Done { .. } | TaskOutcome::NeedsUser { .. }) {
            return Ok(Some(outcome));
        }
        if !self.executor.toolbox.monitors.list().is_empty() {
            self.ui.send(EventKind::GoalVerdict { verdict: "deferred".into(), reason: "background work still running — evaluating when it reports".into() }).await;
            return Ok(Some(outcome));
        }
        if !goal.worked {
            goal.idle += 1;
        } else {
            goal.idle = 0;
        }
        goal.worked = false;
        let model = self.config.goal.model.clone().unwrap_or_else(|| self.config.model.clone());
        let evaluator = match self.secrets.as_ref().map(|s| Provider::for_model(&model, s, &self.config)) {
            Some(Ok(p)) => p,
            _ => {
                self.goal = None;
                self.ui.send(EventKind::GoalVerdict { verdict: "cleared".into(), reason: format!("no credentials for the goal model {model}") }).await;
                self.send_goal().await;
                return Ok(Some(outcome));
            }
        };
        let evidence = goal::evidence(&self.transcript);
        let (verdict, reason) = goal::evaluate(&evaluator, &model, &goal.condition, &evidence).await?;
        goal.turns += 1;
        goal.last_reason = Some(reason.clone());
        let turns = goal.turns;
        let idle = goal.idle;
        let condition = goal.condition.clone();
        let elapsed = goal.started.elapsed().as_secs();
        match verdict {
            goal::Verdict::Met => {
                self.goal = None;
                self.ui.send(EventKind::GoalVerdict { verdict: "met".into(), reason: reason.clone() }).await;
                self.send_goal().await;
                Ok(Some(TaskOutcome::Done { summary: format!("◎ goal met after {turns} turn(s), {}m {}s: {reason}", elapsed / 60, elapsed % 60) }))
            }
            goal::Verdict::Impossible => {
                self.goal = None;
                self.ui.send(EventKind::GoalVerdict { verdict: "impossible".into(), reason: reason.clone() }).await;
                self.send_goal().await;
                Ok(Some(TaskOutcome::Done { summary: format!("✗ goal judged impossible: {reason}") }))
            }
            goal::Verdict::NotYet if idle >= goal::IDLE_LIMIT => {
                self.ui.send(EventKind::GoalVerdict { verdict: "paused".into(), reason: format!("{} turns without using any tool — stopping; the goal stays set, your next message resumes it", goal::IDLE_LIMIT) }).await;
                self.send_goal().await;
                Ok(Some(outcome))
            }
            goal::Verdict::NotYet if turns >= self.config.goal.max_turns => {
                self.goal = None;
                self.ui.send(EventKind::GoalVerdict { verdict: "cleared".into(), reason: format!("{turns} goal turns reached the cap (goal.max_turns)") }).await;
                self.send_goal().await;
                Ok(Some(outcome))
            }
            goal::Verdict::NotYet => {
                self.ui.send(EventKind::GoalVerdict { verdict: "not yet".into(), reason: reason.clone() }).await;
                self.send_goal().await;
                self.transcript.push(Message::User(format!("[goal] not yet met — {reason}\nKeep working toward: {condition}")));
                Ok(None)
            }
        }
    }

    /// Re-read the provider balance in the background and tell the UI.
    /// Throttled mid-task (`force` = task boundaries) so a chatty turn
    /// doesn't become a request per model call.
    fn refresh_balance(&mut self, force: bool) {
        let Some(balance) = self.balance.clone() else { return };
        if !force && self.last_balance_fetch.is_some_and(|t| t.elapsed() < Duration::from_secs(20)) {
            return;
        }
        self.last_balance_fetch = Some(std::time::Instant::now());
        let ui = self.ui.clone();
        tokio::spawn(async move {
            match balance.fetch().await {
                Ok(usd) => ui.send(EventKind::Balance { usd }).await,
                Err(e) => tracing::warn!("balance: {e:#}"),
            }
        });
    }

    async fn send_monitors(&self) {
        let armed = self.executor.toolbox.monitors.list().into_iter().map(|(id, label, _)| (id, label)).collect();
        self.ui.send(EventKind::Monitors { armed }).await;
    }

    /// The pipeline (§5.7): the user's message goes straight into the pool as
    /// a task that may write anywhere in the project, and its report is the
    /// reply. No model reads it first. The next message follows up with the
    /// same pool agent; a task the agent forks runs in parallel inside the
    /// pool. The AFK verify gate and a `/goal` continue the same agent with
    /// what they found, and so does anything typed while it ran.
    async fn run_pipeline(&mut self, instruction: String) -> Result<TaskOutcome> {
        self.task += 1;
        self.executor.toolbox.task = self.task;
        let spawner = self.executor.toolbox.subagents.clone().ok_or_else(|| anyhow::anyhow!("the pipeline needs the pool"))?;
        // A fresh agent (first message, or after a resume) gets the
        // conversation so far; a follow-up already has it.
        let context = if self.pipeline_agent.is_none() { recent_context(&self.transcript) } else { None };
        // Injections waiting since the last reply (plan mode, fill-in) go
        // along with the message.
        let pending: Vec<String> = self
            .transcript
            .iter()
            .rev()
            .take_while(|m| !matches!(m, Message::Assistant { .. }))
            .filter_map(|m| match m {
                Message::User(t) => Some(t.clone()),
                _ => None,
            })
            .collect::<Vec<_>>()
            .into_iter()
            .rev()
            .collect();
        self.transcript.push(Message::User(instruction.clone()));
        self.persist();
        let mut brief = [context, (!pending.is_empty()).then(|| pending.join("\n\n")), Some(instruction)]
            .into_iter()
            .flatten()
            .collect::<Vec<_>>()
            .join("\n\n");
        let mut verify_rounds = 0;
        loop {
            let (n, finished) = spawner.run_pipeline(self.pipeline_agent, brief, &self.ui).await?;
            self.pipeline_agent = Some(n);
            let report = finished.report.trim().to_string();
            self.transcript.push(Message::Assistant { text: report.clone(), tool_calls: vec![] });
            self.persist();
            if *self.cancel.borrow() {
                return Ok(TaskOutcome::Interrupted);
            }
            if let Some(g) = self.goal.as_mut() {
                g.worked = true;
            }
            // Typed while it ran: the next follow-up.
            let mut steered = Vec::new();
            while let Ok(text) = self.steering.try_recv() {
                steered.push(text);
            }
            if !steered.is_empty() {
                brief = steered.join("\n\n");
                self.transcript.push(Message::User(brief.clone()));
                continue;
            }
            if self.afk
                && verify_rounds < 3
                && let Some(red) = self.verify().await?
            {
                verify_rounds += 1;
                self.transcript.push(Message::User(red.clone()));
                brief = red;
                continue;
            }
            let summary = if finished.ok { report } else { format!("✗ unfinished: {report}") };
            match self.goal_gate(TaskOutcome::Done { summary }).await? {
                Some(outcome) => return Ok(outcome),
                // Not met: the gate's `[goal]` note is the next brief.
                None => {
                    brief = match self.transcript.last() {
                        Some(Message::User(t)) => t.clone(),
                        _ => "Keep working toward the goal.".into(),
                    };
                }
            }
        }
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
            if self.deadline.is_some_and(|d| std::time::Instant::now() >= d) {
                return Ok(TaskOutcome::TimeLimit);
            }
            let cancelled = *self.cancel.borrow();
            if cancelled {
                self.repair_transcript("interrupted by user");
                return Ok(TaskOutcome::Interrupted);
            }
            self.inject_steering().await;
            if let Some(halt) = self.enforce_budgets(provider).await? {
                return Ok(halt);
            }

            let mut turn = self.call_model(provider).await?;
            // Canonical argument shapes before the call is stored or run: the
            // model's history then shows it the right shape (tools::normalize).
            for call in &mut turn.tool_calls {
                tools::normalize::canonicalize(call);
            }
            self.transcript.push(Message::Assistant {
                text: turn.text.clone(),
                tool_calls: turn.tool_calls.clone(),
            });

            if turn.tool_calls.is_empty() {
                match self.finish_turn(&turn.text).await? {
                    // A goal keeps the loop going past where the task would end.
                    Some(outcome) => match self.goal_gate(outcome).await? {
                        Some(outcome) => return Ok(outcome),
                        None => continue,
                    },
                    // AFK verify red: [verify] message injected, keep going.
                    None => continue,
                }
            }
            if let Some(g) = self.goal.as_mut() {
                g.worked = true;
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
            // `split` / `fork` (§5.8): the subagent handed back the rest of its
            // brief, in order or in parallel.
            if let Some((done, remaining)) = self.executor.toolbox.split.take() {
                return Ok(TaskOutcome::Split { done, remaining });
            }
            if let Some(f) = self.executor.toolbox.fork.take() {
                return Ok(TaskOutcome::Fork { done: f.done, tasks: f.tasks, then: f.then });
            }

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
    async fn inject_steering(&mut self) {
        while let Ok(text) = self.steering.try_recv() {
            self.transcript.push(Message::User(text));
        }
        // Files this agent read that others changed since (§3.3, §5.8).
        let notes = self.executor.toolbox.changes.lock().unwrap().take_notes(self.id);
        if !notes.is_empty() {
            self.transcript.push(Message::User(format!(
                "[changes] Files you read have changed since — re-read before editing them:\n- {}",
                notes.join("\n- ")
            )));
        }
        let mut events = Vec::new();
        if let Some(rx) = self.monitor_rx.as_mut() {
            while let Ok(event) = rx.try_recv() {
                events.push(event);
            }
        }
        let any = !events.is_empty();
        for event in events {
            self.note_monitor_end(&event);
            let text = event.render();
            self.ui.send(EventKind::MonitorWoke { text: text.clone() }).await;
            self.transcript.push(Message::User(text));
        }
        if any {
            self.send_monitors().await;
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
            self.rewrote = true;
            // Their content left the context: a re-read must send bytes again,
            // not "unchanged since turn N".
            let project = self.executor.toolbox.project.clone();
            self.executor.toolbox.fs.forget(forgotten.iter().map(|f| project.join(f)));
        }
        let session_usd = self.ledger.session_total(self.session).unwrap_or(0.0);
        let month_usd = self.ledger.month_total().unwrap_or(0.0);
        self.ui.send(EventKind::Cost { session_usd, month_usd }).await;
        self.refresh_balance(false);
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
        let schemas = tools::schemas(&self.executor.toolbox.custom, self.executor.toolbox.mask, self.executor.toolbox.denied);

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
            let waited = std::time::Instant::now();
            crate::ratelimit::acquire(&model).await;
            tracing::info!(%model, attempt, waited_ms = waited.elapsed().as_millis() as u64, "model call");
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
                    // A 429 is the provider's rate limit, not a broken model:
                    // stall the model's bucket and wait it out (§7.2).
                    let throttled = e.downcast_ref::<ApiError>().is_some_and(|a| a.status.as_u16() == 429);
                    if throttled && attempt <= 6 {
                        crate::ratelimit::throttled(&model);
                        continue;
                    }
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
    fn record_usage(&mut self, model: &str, usage: Usage) {
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
        self.spent_usd += cost;
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
        match self.verify().await? {
            None => Ok(Some(TaskOutcome::Done { summary: text.to_string() })),
            Some(injection) => {
                self.transcript.push(Message::User(injection));
                Ok(None)
            }
        }
    }

    /// The AFK verify gate's commands (§5.3): None when green or unset, else
    /// the `[verify]` injection naming the red command.
    async fn verify(&mut self) -> Result<Option<String>> {
        if self.config.verify.is_empty() {
            return Ok(None);
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
        Ok(results.iter().find(|r| r.ran && r.exit_code != Some(0)).map(|red| {
            let extract = output::truncate(&format!("{}\n{}", red.stderr, red.stdout), 30);
            prompt::verify_injection(&red.command, red.exit_code.unwrap_or(-1), &extract)
        }))
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
                self.rewrote = true;
            }
        }
        self.transcript = repaired;
    }

    /// Crude estimate (bytes/4) of what each request sends: the prefix, the
    /// tool schemas, and the transcript — fine for the compaction trigger.
    fn context_tokens(&self) -> u64 {
        let schemas = serde_json::to_string(&tools::schemas(&self.executor.toolbox.custom, self.executor.toolbox.mask, self.executor.toolbox.denied))
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

    #[test]
    fn a_subagent_never_writes_the_lead_transcript() {
        let dir = testutil::tmp("persist-child");
        let mut child = test_agent(&dir);
        child.id = AgentId(3);
        child.transcript.push(Message::User("child brief".into()));
        child.persist();
        let ledger = crate::ledger::for_project(&dir);
        assert!(ledger.conversation(child.session, ROOT).is_none(), "the lead's conversation is untouched");
        let own = ledger.conversation(child.session, AgentId(3)).unwrap();
        assert!(matches!(&own.messages[..], [Message::User(t)] if t == "child brief"));
    }

    #[test]
    fn rewritten_history_is_snapshotted_and_replays_exactly() {
        let dir = testutil::tmp("persist-snap");
        let mut a = test_agent(&dir);
        a.transcript.push(Message::User("one".into()));
        a.transcript.push(Message::User("two".into()));
        a.persist();
        a.transcript.push(Message::User("three".into()));
        a.persist();
        // Compaction-style rewrite: history changes in place.
        a.transcript[0] = Message::User("summary of one".into());
        a.rewrote = true;
        a.persist();
        a.transcript.push(Message::User("four".into()));
        a.persist();
        let conv = crate::ledger::for_project(&dir).conversation(a.session, ROOT).unwrap();
        let texts: Vec<String> = conv.messages.iter().map(|m| match m { Message::User(t) => t.clone(), _ => String::new() }).collect();
        assert_eq!(texts, vec!["summary of one", "two", "three", "four"]);
        assert_eq!(crate::ledger::raw(&dir).matches("\"kind\":\"snapshot\"").count(), 1, "appends otherwise");
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
        agent.inject_steering().await;
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

    #[test]
    fn a_fresh_pipeline_agent_gets_the_last_exchanges() {
        assert_eq!(recent_context(&[]), None);
        let mut t = vec![];
        for i in 0..5 {
            t.push(Message::User(format!("ask {i}")));
            t.push(Message::Assistant { text: format!("answer {i}"), tool_calls: vec![] });
        }
        let c = recent_context(&t).unwrap();
        assert!(c.contains("user: ask 2") && c.contains("tursi: answer 4") && !c.contains("ask 1"), "{c}");
        assert!(c.ends_with("[the new message]"));
    }

    #[tokio::test]
    async fn a_task_past_its_deadline_ends_as_a_time_limit_before_any_model_call() {
        let dir = testutil::tmp("deadline");
        let mut agent = afk_agent(&dir);
        agent.deadline = Some(std::time::Instant::now());
        // Nothing listens there: a model call would fail, not time out.
        let provider = Provider::OpenAiCompat(crate::api::openai_compat::Client::new("k".into(), "http://127.0.0.1:9".into()));
        assert!(matches!(agent.run_task(&provider, "do it".into()).await.unwrap(), TaskOutcome::TimeLimit));
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
