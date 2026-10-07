//! rizin tool: reverse-engineering via a persistent, analyzed rizin session
//! (§4). Stateful across calls like `debug` — RE is iterative. The model
//! issues raw rizin commands (afl, pdf/pd, iz, axt, px, s, …) since rizin's
//! command language *is* the interface; enumerating it as typed actions would
//! only limit it.

use anyhow::{Result, anyhow};
use serde::Deserialize;
use serde_json::Value;
use std::path::PathBuf;

use crate::output;
use crate::tools::Toolbox;

#[derive(Deserialize)]
pub struct RizinArgs {
    /// Open (or reopen) this binary and analyze it before running commands.
    pub open: Option<PathBuf>,
    /// Deep analysis (`aaa`) instead of basic (`aa`) — slower on big statics.
    pub deep: Option<bool>,
    /// Raw rizin commands, run in order against the live session.
    #[serde(default)]
    pub commands: Vec<String>,
    /// Close the session when done.
    pub close: Option<bool>,
    /// Per-command output line budget before truncating to the log (default 150).
    pub tail_lines: Option<usize>,
}

/// The last binary analyzed in this project, so a call without a session —
/// a subagent's (sessions aren't shared), or after a resume — reopens it.
fn last_binary_file(project: &std::path::Path) -> PathBuf {
    project.join(".tursi/rizin-last")
}

pub async fn run(tb: &mut Toolbox, args: &Value) -> Result<String> {
    let args: RizinArgs = serde_json::from_value(args.clone())?;
    let mut reopened = String::new();

    if let Some(path) = &args.open {
        if let Some(old) = tb.rizin.take() {
            let _ = old.close().await;
        }
        let bin = if path.is_absolute() { path.clone() } else { tb.project.join(path) };
        if !bin.exists() {
            anyhow::bail!("no such binary: {}", bin.display());
        }
        tb.rizin = Some(crate::rizin::Session::open(&tb.sandbox, &bin, args.deep.unwrap_or(false)).await?);
        let _ = std::fs::write(last_binary_file(&tb.project), bin.display().to_string());
    } else if tb.rizin.is_none() && !args.commands.is_empty() {
        let last = std::fs::read_to_string(last_binary_file(&tb.project)).ok().map(|s| PathBuf::from(s.trim()));
        if let Some(bin) = last.filter(|b| b.exists()) {
            tb.rizin = Some(crate::rizin::Session::open(&tb.sandbox, &bin, false).await?);
            reopened = format!("(no session was open — reopened {}, the last binary analyzed here)\n", bin.display());
        }
    }

    let tail = args.tail_lines.unwrap_or(150);
    let mut out = String::new();
    {
        let session = tb
            .rizin
            .as_mut()
            .ok_or_else(|| anyhow!("no rizin session and no binary analyzed here yet — pass `open` with a binary path"))?;
        if args.commands.is_empty() {
            out.push_str(&format!("rizin session open on {} (analyzed)\n", session.binary));
        }
        for cmd in &args.commands {
            let result = session.cmd(cmd).await?;
            out.push_str(&format!("── {cmd}\n"));
            if result.lines().count() > tail {
                let id = output::log_full(&tb.project, tb.agent, "rizin", &result)?;
                out.push_str(&output::truncate(&result, tail));
                out.push_str(&format!("\n[full: log#{}]\n", id.0));
            } else if result.trim().is_empty() {
                out.push_str("(no output)\n");
            } else {
                out.push_str(&result);
                out.push('\n');
            }
        }
    }

    if args.close.unwrap_or(false) {
        if let Some(session) = tb.rizin.take() {
            session.close().await?;
        }
        out.push_str("(session closed)\n");
    }
    Ok(format!("{reopened}{}", out.trim_end()))
}
