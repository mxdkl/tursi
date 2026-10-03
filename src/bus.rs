//! Channel plumbing between the agent loop and the TUI. The agent never
//! touches the terminal; the TUI never touches the model.
//!
//! Every event carries an `AgentId` per §5.7 — always `ROOT` until subagents
//! exist, but the field exists from day one so swarms are fields, not a
//! migration.

use serde::{Deserialize, Serialize};
use tokio::sync::{mpsc, oneshot};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct AgentId(pub u32);

pub const ROOT: AgentId = AgentId(0);

pub struct UiEvent {
    /// §5.7: every event is attributed. Read once multi-agent rendering
    /// exists; carried from day one so subagents are fields, not migrations.
    #[allow(dead_code)]
    pub agent: AgentId,
    pub kind: EventKind,
}

pub enum EventKind {
    /// Streamed model text delta.
    AgentText(String),
    ToolStarted { name: String, summary: String },
    /// The full result; the TUI collapses it (Ctrl+O expands).
    ToolFinished { name: String, content: String, is_error: bool },
    /// What an edit/write changed, for the transcript's diff view.
    FileDiff(crate::diff::FileDiff),
    /// The one permission prompt: full network for a call (PERMISSIONS.md §4.3).
    Approval(ApprovalRequest),
    /// ask_user overlay (§4.4).
    Ask(AskRequest),
    Cost { session_usd: f64, month_usd: f64 },
    /// Context-window usage for the header gauge: estimated transcript tokens
    /// and the window size (§8).
    Context { used_tokens: u64, window: u64 },
    /// The AFK verify gate started (§5.3).
    Verifying,
    TaskDone { summary: String },
    /// Plan fill-in meter: `stubs 17/23` in the status bar (§5.5).
    StubProgress { filled: usize, total: usize },
    /// A monitor fired while idle: a new turn starts with this text.
    MonitorWoke { text: String },
    /// Armed monitors `(id, label)`, for the status bar.
    Monitors { armed: Vec<(String, String)> },
}

pub struct ApprovalRequest {
    pub summary: String,
    pub diff: Option<String>,
    /// Deny-list hit: render with a warning banner (§3.4).
    pub warn: bool,
    pub reply: oneshot::Sender<ApprovalReply>,
}

pub enum ApprovalReply {
    Approve,
    /// Rejection is steering (§3.4): the optional reason goes into the error
    /// tool result so the model can adapt instead of guessing.
    Reject { reason: Option<String> },
}

pub struct AskRequest {
    pub question: String,
    pub options: Vec<String>,
    pub reply: oneshot::Sender<String>,
}

#[derive(Clone)]
pub struct UiHandle {
    pub agent: AgentId,
    pub tx: mpsc::Sender<UiEvent>,
}

impl UiHandle {
    /// Fire-and-forget: a closed UI (shutdown race) must never wedge a tool.
    pub async fn send(&self, kind: EventKind) {
        let _ = self.tx.send(UiEvent { agent: self.agent, kind }).await;
    }
}
