//! `agent` tool: delegate a brief to a subagent (§5.7) and get back only its
//! report. The child is a full agent loop with a fresh context; the user
//! sees one collapsed entry unless they ask for the trace.

use anyhow::{Result, bail};
use serde::Deserialize;
use serde_json::Value;

use crate::bus::UiHandle;
use crate::tools::Toolbox;

#[derive(Deserialize)]
pub struct AgentArgs {
    /// explore | review | worker
    pub role: String,
    /// What to do and what to report back. Self-contained: the child knows
    /// nothing of this conversation.
    pub brief: String,
}

pub async fn run(tb: &mut Toolbox, args: &Value, ui: &UiHandle) -> Result<String> {
    let args: AgentArgs = serde_json::from_value(args.clone())?;
    let Some(spawner) = tb.subagents.clone() else {
        bail!("subagents can't spawn subagents — do this part yourself");
    };
    let role = crate::agent::Role::parse(&args.role)?;
    if args.brief.trim().len() < 20 {
        bail!("the brief is too short to work from — say what to do, where to look, and what to report");
    }
    spawner.run(role, args.brief, ui).await
}
