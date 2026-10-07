//! `monitor` tool: arm a watch (files under the project, or a command's
//! output) and get woken when it fires — instead of polling with sleeps.
//! Session-scoped; see `crate::monitor`.

use anyhow::{Result, bail};
use serde::Deserialize;
use serde_json::Value;
use std::path::PathBuf;
use std::time::Duration;

use crate::bus::{EventKind, UiHandle};
use crate::tools::Toolbox;

/// Monitors armed with no one around (AFK/headless) time out after this.
const AFK_DEFAULT_TIMEOUT: Duration = Duration::from_secs(30 * 60);

#[derive(Deserialize, Default)]
pub struct MonitorArgs {
    /// "paths" or "command".
    pub watch: Option<String>,
    pub paths: Option<Vec<PathBuf>>,
    /// File-name wildcard for `paths` (e.g. "*.md").
    pub pattern: Option<String>,
    pub command: Option<String>,
    /// Short name shown in wake messages.
    pub label: Option<String>,
    /// Remove the monitor after this long (default: never).
    pub timeout_seconds: Option<u64>,
    /// Stop the monitor with this id.
    pub stop: Option<String>,
    /// List armed monitors.
    pub list: Option<bool>,
}

pub async fn run(tb: &mut Toolbox, args: &Value, ui: &UiHandle) -> Result<String> {
    let args: MonitorArgs = serde_json::from_value(args.clone())?;
    // Unattended, a monitor must not wait forever: default a timeout.
    let timeout = args.timeout_seconds.map(Duration::from_secs).or(if tb.afk { Some(AFK_DEFAULT_TIMEOUT) } else { None });
    let out = if let Some(id) = args.stop {
        if tb.monitors.stop(&id) { format!("monitor {id} stopped") } else { bail!("no monitor {id}") }
    } else if args.list.unwrap_or(false) || args.watch.is_none() {
        match tb.monitors.list().as_slice() {
            [] => "no monitors armed".to_string(),
            list => list.iter().map(|(id, label, kind)| format!("{id} {label} ({kind})")).collect::<Vec<_>>().join("\n"),
        }
    } else {
        match args.watch.as_deref() {
            Some("paths") => {
                let paths = args.paths.unwrap_or_default();
                let label = args.label.unwrap_or_else(|| paths.first().map(|p| p.display().to_string()).unwrap_or_default());
                let id = tb.monitors.watch_paths(paths, args.pattern, label.clone(), timeout)?;
                format!("{id} armed: {label} — end your turn; you'll be woken when files change")
            }
            Some("command") => {
                let command = args.command.ok_or_else(|| anyhow::anyhow!("command monitors need `command`"))?;
                let label = args.label.unwrap_or_else(|| command.chars().take(30).collect());
                let watch = Some(crate::changes::watch(&tb.changes, tb.agent, tb.task, &command));
                let id = tb.monitors.watch_command(command, label.clone(), timeout, false, watch).await?;
                format!("{id} armed: {label} — end your turn; you'll be woken with its output and exit")
            }
            Some(other) => bail!("watch must be \"paths\" or \"command\", got {other:?}"),
            None => unreachable!(),
        }
    };
    ui.send(EventKind::Monitors { armed: tb.monitors.list().into_iter().map(|(id, label, _)| (id, label)).collect() }).await;
    Ok(out)
}
