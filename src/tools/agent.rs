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
    /// Files or directories the subagent may change (`.`: anywhere in the
    /// project). None or empty: a reader, which cannot change the project.
    /// What kind of work it is the harness works out from the brief (§5.8).
    pub writes: Option<Vec<String>>,
    /// Capabilities the model must have: vision, reasoning, long_context.
    #[serde(default)]
    pub needs: Vec<String>,
    /// `agent-N` from an earlier report — follow up with that subagent, its
    /// context intact, instead of starting a new one.
    pub agent: Option<String>,
    /// What to do and what to report back. Self-contained for a new child;
    /// a follow-up may refer to the child's earlier work.
    pub brief: String,
}

pub async fn run(tb: &mut Toolbox, args: &Value, ui: &UiHandle) -> Result<String> {
    let args: AgentArgs = serde_json::from_value(args.clone())?;
    let Some(spawner) = tb.subagents.clone() else {
        bail!("subagents can't spawn subagents — do this part yourself");
    };
    if args.brief.trim().len() < 20 {
        bail!("the brief is too short to work from — say what to do, where to look, and what to report");
    }
    let writes = args.writes.as_deref().map(|w| super::areas(&tb.project, w, true)).transpose()?;
    // Always in the background: a lead that could block would always block,
    // and the point is to keep it working while children run.
    let out = if let Some(agent) = args.agent.as_deref().map(str::trim).filter(|a| !a.is_empty()) {
        let n: u32 = agent
            .trim_start_matches("agent-")
            .parse()
            .map_err(|_| anyhow::anyhow!("agent must be an id from an earlier report, like agent-2"))?;
        spawner.continue_in_background(n, args.brief, writes, ui, &mut tb.monitors).await?
    } else {
        spawner.start(writes.unwrap_or_default(), args.needs, args.brief, ui, &mut tb.monitors).await?
    };
    ui.send(crate::bus::EventKind::Monitors { armed: tb.monitors.list().into_iter().map(|(id, label, _)| (id, label)).collect() }).await;
    Ok(out)
}
