//! Non-interactive execution: run one task unattended and report metrics as
//! JSON. This is the entry point the benchmark runner (BENCH.md) drives, and
//! it's independently useful for scripting/CI.
//!
//! Always AFK: no overlays, no questions. The verify gate (§5.3) decides
//! "done". The one prompt that exists (full network, PERMISSIONS.md §4.3) is
//! auto-rejected — headless must never block on a human.

use anyhow::Result;
use std::path::PathBuf;
use std::time::Instant;
use tokio::sync::{mpsc, watch};

use crate::agent::{self, Command};
use crate::bus::{ApprovalReply, EventKind, ROOT, UiEvent, UiHandle};
use crate::config::{Config, Secrets};
use crate::session::Session;
use crate::syscard::SystemCard;

/// What a headless run does: one instruction, or a goal it works toward.
pub enum Job {
    Task(String),
    Goal(String),
}

#[allow(clippy::too_many_arguments)]
pub async fn run(
    project: PathBuf,
    config: Config,
    secrets: Secrets,
    card: SystemCard,
    session: Session,
    sandbox: crate::sandbox::Sandbox,
    job: Job,
    json: bool,
) -> Result<()> {
    let (event_tx, mut event_rx) = mpsc::channel::<UiEvent>(256);
    let (_steer_tx, steer_rx) = mpsc::channel::<String>(1);
    let (_cancel_tx, cancel_rx) = watch::channel(false);
    let (cmd_tx, cmd_rx) = mpsc::channel::<Command>(4);

    let session_id = session.id;
    let agent = agent::build(
        project,
        config,
        &card,
        session_id,
        sandbox,
        secrets.clone(),
        true, // afk
        true, // headless: edit in place
        session.resumed,
        UiHandle { agent: ROOT, tx: event_tx },
        steer_rx,
        cancel_rx,
    )?;
    let ledger = crate::stats::Ledger::open()?;
    let serve = tokio::spawn(agent.serve(secrets, cmd_rx));

    // Drain events: auto-reject any approval (nothing can answer headless),
    // mirror progress to stderr, and report each task's end together with
    // what's still armed — monitors keep a headless run alive (they time out
    // on their own, §monitor) until none remain.
    let (done_tx, mut done_rx) = mpsc::channel::<(String, usize, Option<f64>)>(8);
    let drain = tokio::spawn(async move {
        let mut armed = 0usize;
        let mut balance: Option<f64> = None;
        while let Some(ev) = event_rx.recv().await {
            let indent = if ev.agent == ROOT { "" } else { "    " };
            match ev.kind {
                EventKind::Approval(req) => {
                    let _ = req.reply.send(ApprovalReply::Reject {
                        reason: Some("headless: no one to approve".into()),
                    });
                }
                EventKind::Ask(req) => {
                    let _ = req.reply.send("(headless: no user)".into());
                }
                EventKind::ToolStarted { name, summary } => eprintln!("{indent}  ▸ {name} {summary}"),
                EventKind::ToolFinished { name, content, is_error } => {
                    let first = content.lines().next().unwrap_or("").chars().take(100).collect::<String>();
                    eprintln!("{indent}  {} {name}: {first}", if is_error { '✗' } else { '✓' });
                }
                EventKind::Monitors { armed: list } => armed = list.len(),
                EventKind::Balance { usd } => balance = Some(usd),
                EventKind::GoalVerdict { verdict, reason } => eprintln!("  ◎ goal {verdict} — {reason}"),
                EventKind::MonitorWoke { text } => eprintln!("  ⚡ {}", text.lines().next().unwrap_or("")),
                EventKind::SubagentStarted { access, title, model, continued } => {
                    eprintln!("  ▶ agent-{} {access} ({model}){}: {title}", ev.agent.0, if continued { " (follow-up)" } else { "" })
                }
                EventKind::SubagentFinished { ok, .. } => eprintln!("  ■ agent-{} {}", ev.agent.0, if ok { "finished" } else { "stopped" }),
                EventKind::TaskDone { summary } if ev.agent != ROOT => eprintln!("{indent}  ● (subagent) {}", summary.lines().next().unwrap_or("")),
                EventKind::TaskDone { summary } => {
                    eprintln!("  ● {summary}");
                    let _ = done_tx.send((summary, armed, balance)).await;
                }
                _ => {}
            }
        }
    });

    let started = Instant::now();
    cmd_tx
        .send(match job {
            Job::Task(task) => Command::Task(task),
            Job::Goal(condition) => Command::GoalSet(condition),
        })
        .await?;
    let mut summary = "(no completion)".to_string();
    let mut balance: Option<f64> = None;
    while let Some((s, armed, bal)) = done_rx.recv().await {
        summary = s;
        balance = bal.or(balance);
        if armed == 0 {
            break;
        }
        eprintln!("  ⏱ {armed} monitor(s) armed — waiting");
    }
    let wall_ms = started.elapsed().as_millis();
    drop(cmd_tx); // closes serve's command channel
    let _ = serve.await;
    drain.abort();

    let s = ledger.session_stats(session_id).unwrap_or_default();
    // "done" iff the summary isn't one of the failure/limit markers serve emits.
    let ok = !summary.starts_with('✗');
    if json {
        println!(
            "{}",
            serde_json::json!({
                "outcome": if ok { "done" } else { "failed" },
                "summary": summary,
                "model_calls": s.model_calls,
                "input_tokens": s.input_tokens,
                "cached_tokens": s.cached_tokens,
                "output_tokens": s.output_tokens,
                "cost_usd": s.cost_usd,
                "balance_usd": balance,
                "wall_ms": wall_ms,
            })
        );
    } else {
        println!(
            "outcome={} calls={} in={} cached={} out={} cost=${:.4} wall={}ms",
            if ok { "done" } else { "failed" },
            s.model_calls,
            s.input_tokens,
            s.cached_tokens,
            s.output_tokens,
            s.cost_usd,
            wall_ms,
        );
    }
    session.close()?;
    Ok(())
}
