//! Ratatui TUI: hand-rolled modal layer per the newbbs pattern (§2). The UI
//! owns the terminal and the App; the agent runs in its own task and talks
//! over the bus. Keys arrive from a blocking reader thread — no extra deps.

pub mod command;
pub mod keys;
pub mod render;
pub mod transcript;

use anyhow::Result;
use std::path::PathBuf;
use tokio::sync::{mpsc, watch};

use crate::agent;
use crate::bus::{ApprovalRequest, AskRequest, EventKind, ROOT, UiEvent, UiHandle};
use crate::config::{Config, Secrets};
use crate::session::Session;
use crate::syscard::SystemCard;
use transcript::{Entry, ToolResult};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    Normal,
    Insert,
    Command,
}

pub enum Overlay {
    /// The network prompt (PERMISSIONS.md §4.3): y / n(+reason).
    Approval(ApprovalRequest),
    /// Typing a rejection reason — rejection is steering (§3.4).
    RejectReason(ApprovalRequest, String),
    /// Question + numbered options + free-text buffer (§4.4).
    Ask(AskRequest, String),
    Help,
    SysInfo,
}

pub struct App {
    // Modal state (newbbs pattern).
    pub mode: Mode,
    pub input: String,
    pub cursor: usize,
    pub command: String,
    pub status: Option<String>,
    pub scroll: usize,
    /// Two-key sequences; `gg` is the only one.
    pub pending: Option<char>,
    pub overlay: Option<Overlay>,
    pub quitting: bool,

    // Harness state mirrored for rendering.
    pub afk: bool,
    pub plan_active: bool,
    /// (filled, total) fill-in meter (§5.5).
    pub stubs: Option<(usize, usize)>,
    /// Armed monitors `(id, label)` for the status bar.
    pub monitors: Vec<(String, String)>,
    pub session_usd: f64,
    pub month_usd: f64,
    /// Context-window gauge for the header: estimated transcript tokens and the
    /// window size (§8).
    pub ctx_used: u64,
    pub ctx_window: u64,
    pub model: String,
    pub task_running: bool,
    /// The structured scrollback (`ui::transcript`), bottom-anchored.
    pub transcript: Vec<Entry>,
    /// Model text streamed for the current block, not yet closed by a tool
    /// call or the task end.
    pub partial: String,
    /// Ctrl+O: show every line of tool results instead of the first few.
    pub verbose: bool,
    /// When the running task started and the session spend then, for the
    /// `✻` footer's elapsed time and per-task cost.
    pub task_started: Option<std::time::Instant>,
    pub task_start_usd: f64,
    /// Set by the renderer each frame; scroll math needs them.
    pub view_height: usize,
    pub line_count: usize,

    pub session: Session,
    pub card: SystemCard,

    // Channels to/from the agent (§5.2).
    pub events: mpsc::Receiver<UiEvent>,
    pub steering: mpsc::Sender<String>,
    pub cancel: watch::Sender<bool>,
    pub cmd: mpsc::Sender<agent::Command>,
}

/// Bootstrap channels + toolbox + the agent task, enter the terminal, drive
/// key events and bus events under one select loop. Restores the terminal on
/// every exit path (ratatui's init installs the panic hook).
pub async fn run(
    project: PathBuf,
    config: Config,
    secrets: Secrets,
    card: SystemCard,
    session: Session,
    sandbox: crate::sandbox::Sandbox,
    afk: bool,
) -> Result<()> {
    let (event_tx, event_rx) = mpsc::channel::<UiEvent>(256);
    let (steer_tx, steer_rx) = mpsc::channel::<String>(16);
    let (cancel_tx, cancel_rx) = watch::channel(false);
    let (cmd_tx, cmd_rx) = mpsc::channel::<agent::Command>(16);

    let agent = agent::build(
        project.clone(),
        config.clone(),
        &card,
        session.id,
        sandbox,
        afk,
        false,
        session.resumed,
        UiHandle { agent: ROOT, tx: event_tx.clone() },
        steer_rx,
        cancel_rx,
    )?;
    tokio::spawn(agent.serve(secrets, cmd_rx));

    // Blocking key reader thread → async channel; dies with the channel.
    let (key_tx, mut key_rx) = mpsc::channel::<crossterm::event::Event>(64);
    std::thread::spawn(move || {
        while let Ok(event) = crossterm::event::read() {
            if key_tx.blocking_send(event).is_err() {
                break;
            }
        }
    });

    let model = config.model.clone();
    let mut app = App {
        mode: Mode::Normal,
        input: String::new(),
        cursor: 0,
        command: String::new(),
        status: Some("i to type, : for commands, :help for the keymap".to_string()),
        scroll: 0,
        pending: None,
        overlay: None,
        quitting: false,
        afk,
        plan_active: false,
        stubs: None,
        monitors: Vec::new(),
        session_usd: 0.0,
        month_usd: 0.0,
        ctx_used: 0,
        ctx_window: crate::agent::prompt::DEFAULT_CONTEXT_WINDOW,
        model,
        task_running: false,
        transcript: Vec::new(),
        partial: String::new(),
        verbose: false,
        task_started: None,
        task_start_usd: 0.0,
        view_height: 24,
        line_count: 0,
        session,
        card,
        events: event_rx,
        steering: steer_tx,
        cancel: cancel_tx,
        cmd: cmd_tx,
    };

    if app.session.resumed {
        // Replay what the model remembers, so the screen matches its context.
        let path = Session::transcript_path(&project, app.session.id);
        if let Some(messages) = std::fs::read_to_string(&path)
            .ok()
            .and_then(|t| serde_json::from_str::<serde_json::Value>(&t).ok())
            .and_then(|v| serde_json::from_value::<Vec<crate::api::Message>>(v.get("messages")?.clone()).ok())
        {
            app.transcript = transcript::from_messages(&messages);
        }
        app.transcript.push(Entry::Note(format!("── resumed session {} ──", app.session.id)));
    }
    use std::io::IsTerminal;
    if !std::io::stdout().is_terminal() {
        anyhow::bail!("tursi is a TUI — run it in a terminal");
    }
    let mut terminal = ratatui::init();
    let result = loop {
        if let Err(e) = terminal.draw(|frame| render::draw(frame, &mut app)) {
            break Err(e.into());
        }
        tokio::select! {
            key = key_rx.recv() => match key {
                Some(crossterm::event::Event::Key(key)) => {
                    if let Err(e) = keys::handle(&mut app, key).await {
                        app.set_error(format!("{e:#}"));
                    }
                }
                Some(_) => {} // resize redraws on next frame
                None => break Ok(()),
            },
            event = app.events.recv() => match event {
                Some(event) => {
                    app.apply_event(event);
                    // Batch whatever else already arrived before redrawing.
                    while let Ok(more) = app.events.try_recv() {
                        app.apply_event(more);
                    }
                }
                None => break Ok(()),
            },
        }
        if app.quitting {
            break Ok(());
        }
    };
    ratatui::restore();
    app.session.close()?;
    result
}

impl App {
    pub fn quit(&mut self) {
        self.quitting = true;
    }

    pub fn set_status(&mut self, s: impl Into<String>) {
        self.status = Some(s.into());
    }

    /// Command failures land here — never tear the session down (newbbs rule).
    pub fn set_error(&mut self, s: impl Into<String>) {
        self.status = Some(format!("✗ {}", s.into()));
    }

    /// Insert-mode Enter: steering if a task runs, otherwise a new task (§5.2).
    pub async fn send_message(&mut self) -> Result<()> {
        let text = std::mem::take(&mut self.input);
        self.cursor = 0;
        let text = text.trim().to_string();
        if text.is_empty() {
            return Ok(());
        }
        self.transcript.push(Entry::User(text.clone()));
        if self.task_running {
            let _ = self.steering.send(text).await;
            self.set_status("queued as steering for the next iteration");
        } else if self.cmd.send(agent::Command::Task(text)).await.is_err() {
            // The agent task is gone (panic?) — never leave a silent hang.
            self.set_error("agent task is gone — restart tursi (.tursi/harness.log has details)");
        } else {
            self.task_running = true;
            self.task_started = Some(std::time::Instant::now());
            self.task_start_usd = self.session_usd;
            self.scroll = 0;
            let _ = self.session.transition(crate::session::State::Running);
        }
        Ok(())
    }

    /// Esc while running (§5.2): flip the cancel watch; the loop repairs the
    /// transcript and returns Interrupted.
    pub fn interrupt(&mut self) {
        let _ = self.cancel.send(true);
        self.set_status("interrupting — waiting for the loop to unwind…");
    }

    /// Apply one agent event to the view.
    pub fn apply_event(&mut self, event: UiEvent) {
        match event.kind {
            EventKind::AgentText(delta) => self.partial.push_str(&delta),
            EventKind::ToolStarted { name, summary } => {
                self.flush_partial();
                self.transcript.push(Entry::Tool { name, summary, result: None });
            }
            EventKind::ToolFinished { name, content, is_error } => {
                // Results land on the newest unanswered call with that name.
                let slot = self.transcript.iter_mut().rev().find_map(|e| match e {
                    Entry::Tool { name: n, result, .. } if *n == name && result.is_none() => Some(result),
                    _ => None,
                });
                match slot {
                    Some(result) => *result = Some(ToolResult { content, is_error }),
                    None => self.transcript.push(Entry::Tool { name, summary: String::new(), result: Some(ToolResult { content, is_error }) }),
                }
            }
            EventKind::FileDiff(diff) => self.transcript.push(Entry::Diff(diff)),
            EventKind::Approval(request) => {
                bell();
                let _ = self.session.transition(crate::session::State::AwaitingApproval);
                self.overlay = Some(Overlay::Approval(request));
            }
            EventKind::Ask(request) => {
                bell();
                let _ = self.session.transition(crate::session::State::AwaitingUser);
                self.overlay = Some(Overlay::Ask(request, String::new()));
            }
            EventKind::Cost { session_usd, month_usd } => {
                self.session_usd = session_usd;
                self.month_usd = month_usd;
            }
            EventKind::Context { used_tokens, window } => {
                self.ctx_used = used_tokens;
                self.ctx_window = window;
            }
            EventKind::Verifying => {
                let _ = self.session.transition(crate::session::State::Verifying);
                self.set_status("verify gate running…");
            }
            EventKind::TaskDone { summary } => {
                self.flush_partial();
                // The final prose usually streamed already; only show the
                // summary when it says something else (a limit, an error).
                let shown = matches!(self.transcript.last(), Some(Entry::Agent(t)) if t.trim() == summary.trim());
                if !shown && !summary.trim().is_empty() {
                    self.transcript.push(Entry::Agent(summary.clone()));
                }
                self.transcript.push(Entry::Done {
                    ok: !summary.starts_with('✗'),
                    elapsed: self.task_started.take().map(|t| t.elapsed()).unwrap_or_default(),
                    at: chrono::Local::now().format("%-I:%M %p").to_string(),
                    cost_usd: (self.session_usd - self.task_start_usd).max(0.0),
                    ctx_tokens: self.ctx_used,
                });
                self.task_running = false;
                let _ = self.cancel.send(false);
                let _ = self.session.transition(crate::session::State::Idle);
            }
            EventKind::StubProgress { filled, total } => {
                self.stubs = Some((filled, total));
            }
            EventKind::MonitorWoke { text } => {
                self.flush_partial();
                self.transcript.push(Entry::Wake(text));
                self.task_running = true;
                self.task_started = Some(std::time::Instant::now());
                self.task_start_usd = self.session_usd;
                self.scroll = 0;
                let _ = self.session.transition(crate::session::State::Running);
            }
            EventKind::Monitors { armed } => self.monitors = armed,
        }
    }

    fn flush_partial(&mut self) {
        let text = std::mem::take(&mut self.partial);
        if !text.trim().is_empty() {
            self.transcript.push(Entry::Agent(text.trim_end().to_string()));
        }
    }
}

fn bell() {
    use std::io::Write;
    let _ = std::io::stdout().write_all(b"\x07");
}

#[cfg(test)]
pub(crate) mod testui {
    use super::*;
    use crate::tools::testutil;

    /// A fully-wired App over a tmp project, plus the far ends of its channels.
    pub fn app(
        name: &str,
    ) -> (App, mpsc::Sender<UiEvent>, mpsc::Receiver<String>, watch::Receiver<bool>, mpsc::Receiver<agent::Command>)
    {
        let dir = testutil::tmp(name);
        let (event_tx, event_rx) = mpsc::channel(64);
        let (steer_tx, steer_rx) = mpsc::channel(16);
        let (cancel_tx, cancel_rx) = watch::channel(false);
        let (cmd_tx, cmd_rx) = mpsc::channel(16);
        let session = Session::open_or_resume(&dir, None).unwrap();
        let card = crate::syscard::SystemCard {
            cpu: "test".into(),
            isa: vec![],
            mem: "1 GiB".into(),
            gpus: vec![],
            os: "test os".into(),
            toolchains: vec![],
            caps: crate::syscard::Capabilities {
                sandbox: false,
                perf_event_paranoid: None,
                ptrace_scope: None,
                gdb_dap: false,
                rr: false,
                profile_backends: vec![],
            },
        };
        let app = App {
            mode: Mode::Normal,
            input: String::new(),
            cursor: 0,
            command: String::new(),
            status: None,
            scroll: 0,
            pending: None,
            overlay: None,
            quitting: false,
            afk: false,
            plan_active: false,
            stubs: None,
            monitors: Vec::new(),
            session_usd: 0.0,
            month_usd: 0.0,
            ctx_used: 0,
            ctx_window: crate::agent::prompt::DEFAULT_CONTEXT_WINDOW,
            model: "test/model".into(),
            task_running: false,
            transcript: Vec::new(),
            partial: String::new(),
            verbose: false,
            task_started: None,
            task_start_usd: 0.0,
            view_height: 24,
            line_count: 0,
            session,
            card,
            events: event_rx,
            steering: steer_tx,
            cancel: cancel_tx,
            cmd: cmd_tx,
        };
        (app, event_tx, steer_rx, cancel_rx, cmd_rx)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn streamed_text_becomes_a_block_and_results_attach_to_their_call() {
        let (mut app, ..) = testui::app("ui-stream");
        app.apply_event(UiEvent { agent: ROOT, kind: EventKind::AgentText("hel".into()) });
        app.apply_event(UiEvent { agent: ROOT, kind: EventKind::AgentText("lo\nworld".into()) });
        assert_eq!(app.partial, "hello\nworld");
        app.apply_event(UiEvent { agent: ROOT, kind: EventKind::ToolStarted { name: "read".into(), summary: "a.rs".into() } });
        assert!(matches!(&app.transcript[0], Entry::Agent(t) if t == "hello\nworld"), "partial flushed as a block");
        app.apply_event(UiEvent { agent: ROOT, kind: EventKind::ToolFinished { name: "read".into(), content: "1→x".into(), is_error: false } });
        assert!(matches!(&app.transcript[1], Entry::Tool { result: Some(r), .. } if r.content == "1→x"));
        app.task_running = true;
        app.task_started = Some(std::time::Instant::now());
        app.apply_event(UiEvent { agent: ROOT, kind: EventKind::AgentText("all done".into()) });
        app.apply_event(UiEvent { agent: ROOT, kind: EventKind::TaskDone { summary: "all done".into() } });
        // The streamed prose is the summary: shown once, then the footer.
        assert!(matches!(&app.transcript[2], Entry::Agent(t) if t == "all done"));
        assert!(matches!(app.transcript.last(), Some(Entry::Done { ok: true, .. })));
        assert_eq!(app.transcript.len(), 4);
        assert!(!app.task_running);
    }

    #[tokio::test]
    async fn approval_event_raises_overlay_and_y_replies_approve() {
        let (mut app, ..) = testui::app("ui-approve");
        let (reply, rx) = tokio::sync::oneshot::channel();
        app.apply_event(UiEvent {
            agent: ROOT,
            kind: EventKind::Approval(ApprovalRequest {
                summary: "edit x".into(),
                diff: Some("- a\n+ b".into()),
                warn: false,
                reply,
            }),
        });
        assert!(matches!(app.overlay, Some(Overlay::Approval(_))));
        keys::handle(&mut app, keys::press('y')).await.unwrap();
        assert!(app.overlay.is_none());
        assert!(matches!(rx.await.unwrap(), crate::bus::ApprovalReply::Approve));
    }
}
