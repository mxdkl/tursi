//! debug: ordered action lists against the DAP session (§4.3). The Action
//! enum is the full surface of the tool.

use anyhow::Result;
use serde::Deserialize;
use serde_json::Value;
use std::time::Duration;

use crate::tools::Toolbox;

#[derive(Deserialize)]
pub struct DebugArgs {
    pub actions: Vec<Action>,
}

#[derive(Deserialize)]
#[serde(tag = "action", rename_all = "snake_case")]
pub enum Action {
    // Session — all inside the sandbox (PERMISSIONS.md §2.2).
    Launch {
        program: String,
        args: Option<Vec<String>>,
        record: Option<bool>,
        /// "main" (default) or "entry" — use "entry" for stripped binaries.
        stop_at: Option<String>,
    },
    Attach { pid: u32 },
    OpenCore { program: String, core: String },
    Quit,
    // Breakpoints.
    Break { location: String, condition: Option<String> },
    Watch { expr: String },
    // Execution — each resuming action takes a timeout cap (§4.3).
    Continue { timeout_seconds: Option<u64> },
    Step,
    Next,
    Finish,
    /// rr replays only.
    Rcontinue { timeout_seconds: Option<u64> },
    Rstep,
    // Inspection.
    Stack { max_frames: Option<usize> },
    Locals { frame: Option<u32> },
    Eval { expr: String, frame: Option<u32> },
    Registers,
    ReadMemory { addr: String, len: usize },
    /// Dump a raw memory range to a file for offline analysis (JIT'd code, etc.)
    DumpMemory { addr: String, len: usize, path: String },
    Disassemble { at: String, count: Option<usize> },
    /// Raw gdb console escape hatch.
    Command { text: String },
}

/// Execute in order, stop on first error (§4.3). Session-creating actions
/// fill `tb.debugger`; the rest require it live. Resuming actions return
/// compact stop reports, never the raw transcript (which goes to debug.log).
pub async fn run(tb: &mut Toolbox, args: &Value) -> Result<String> {
    use crate::dap::{Session, StepKind};
    let args: DebugArgs = serde_json::from_value(args.clone())?;
    if args.actions.is_empty() {
        anyhow::bail!("no actions given");
    }
    let mut out = String::new();
    for action in args.actions {
        let label = label(&action);
        let result: Result<String> = match action {
            Action::Launch { program, args: prog_args, record, stop_at } => {
                if tb.debugger.is_some() {
                    Err(anyhow::anyhow!("a debug session is already live — quit it first"))
                } else {
                    let program = absolutize(tb, &program);
                    let session = Session::launch(
                        &tb.sandbox,
                        &program,
                        prog_args.unwrap_or_default(),
                        record.unwrap_or(false),
                        stop_at.as_deref(),
                    )
                    .await?;
                    let where_ = if stop_at.as_deref() == Some("entry") { "entry point" } else { "main" };
                    let report = render_where(&mut tb.debugger.insert(session).stack(3).await?);
                    Ok(format!("launched, stopped at {where_}\n{report}"))
                }
            }
            // Attach reaches only processes inside the sandbox — the agent's
            // own (PERMISSIONS.md §2.3) — so it needs no prompt.
            Action::Attach { pid } => {
                if tb.debugger.is_some() {
                    Err(anyhow::anyhow!("a debug session is already live — quit it first"))
                } else {
                    let session = Session::attach(&tb.sandbox, pid).await?;
                    tb.debugger = Some(session);
                    Ok(format!("attached to pid {pid}"))
                }
            }
            Action::OpenCore { program, core } => {
                if tb.debugger.is_some() {
                    Err(anyhow::anyhow!("a debug session is already live — quit it first"))
                } else {
                    let session = Session::open_core(&tb.sandbox, &absolutize(tb, &program), &absolutize(tb, &core)).await?;
                    tb.debugger = Some(session);
                    Ok("core loaded — stack/locals/eval available".to_string())
                }
            }
            Action::Quit => match tb.debugger.take() {
                Some(session) => {
                    let transcript = flush_output(tb, session).await?;
                    Ok(format!("session closed{transcript}"))
                }
                None => Ok("no live session".to_string()),
            },
            other => {
                let session = tb.debugger.as_mut().ok_or_else(|| {
                    anyhow::anyhow!("no live debug session — launch/attach/open_core first")
                })?;
                let timeout = |secs: Option<u64>| Duration::from_secs(secs.unwrap_or(30));
                match other {
                    Action::Break { location, condition } => {
                        session.set_breakpoint(&location, condition.as_deref()).await
                    }
                    Action::Watch { expr } => session.set_watchpoint(&expr).await,
                    Action::Continue { timeout_seconds } => {
                        session.resume(StepKind::Continue, timeout(timeout_seconds)).await.map(render_report)
                    }
                    Action::Step => session.resume(StepKind::Step, timeout(None)).await.map(render_report),
                    Action::Next => session.resume(StepKind::Next, timeout(None)).await.map(render_report),
                    Action::Finish => session.resume(StepKind::Finish, timeout(None)).await.map(render_report),
                    Action::Rcontinue { timeout_seconds } => session
                        .resume(StepKind::ReverseContinue, timeout(timeout_seconds))
                        .await
                        .map(render_report),
                    Action::Rstep => {
                        session.resume(StepKind::ReverseStep, timeout(None)).await.map(render_report)
                    }
                    Action::Stack { max_frames } => session.stack(max_frames.unwrap_or(10)).await,
                    Action::Locals { frame } => session.locals(frame).await,
                    Action::Eval { expr, frame } => session.eval(&expr, frame).await,
                    Action::Registers => session.registers().await,
                    Action::ReadMemory { addr, len } => session.read_memory(&addr, len).await,
                    Action::DumpMemory { addr, len, path } => session.dump_memory(&addr, len, &path).await,
                    Action::Disassemble { at, count } => session.disassemble(&at, count.unwrap_or(30)).await,
                    Action::Command { text } => session.command(&text).await,
                    Action::Launch { .. } | Action::Attach { .. } | Action::OpenCore { .. } | Action::Quit => {
                        unreachable!("handled above")
                    }
                }
            }
        };
        match result {
            Ok(text) => {
                out.push_str(&format!("── {label}\n{text}\n"));
            }
            Err(e) => {
                // Stop on first error (§4.3); earlier results still report.
                let done = if out.is_empty() { String::new() } else { format!("{out}\n") };
                anyhow::bail!("{done}{label} failed: {e:#}");
            }
        }
    }
    // Target output goes to the log, never the transcript (§4.3).
    if let Some(session) = tb.debugger.as_mut() {
        let output = session.take_output();
        if !output.trim().is_empty() {
            let id = crate::output::log_full(&tb.project, tb.agent, "debug", &output)?;
            out.push_str(&format!("[target output: log#{}]\n", id.0));
        }
    }
    Ok(out.trim_end().to_string())
}

fn label(action: &Action) -> String {
    match action {
        Action::Launch { program, stop_at, .. } => match stop_at.as_deref() {
            Some(at) => format!("launch {program} (stop_at {at})"),
            None => format!("launch {program}"),
        },
        Action::Attach { pid } => format!("attach {pid}"),
        Action::OpenCore { core, .. } => format!("open_core {core}"),
        Action::Break { location, .. } => format!("break {location}"),
        Action::Watch { expr } => format!("watch {expr}"),
        Action::Continue { .. } => "continue".to_string(),
        Action::Step => "step".to_string(),
        Action::Next => "next".to_string(),
        Action::Finish => "finish".to_string(),
        Action::Rcontinue { .. } => "rcontinue".to_string(),
        Action::Rstep => "rstep".to_string(),
        Action::Stack { .. } => "stack".to_string(),
        Action::Locals { .. } => "locals".to_string(),
        Action::Eval { expr, .. } => format!("eval {expr}"),
        Action::Registers => "registers".to_string(),
        Action::ReadMemory { addr, .. } => format!("read_memory {addr}"),
        Action::DumpMemory { addr, path, .. } => format!("dump_memory {addr} -> {path}"),
        Action::Disassemble { at, .. } => format!("disassemble {at}"),
        Action::Command { text } => format!("command {text}"),
        Action::Quit => "quit".to_string(),
    }
}

fn render_report(report: crate::dap::StopReport) -> String {
    use crate::dap::StopReason;
    let reason = match &report.reason {
        StopReason::Breakpoint => "breakpoint".to_string(),
        StopReason::Watchpoint => "watchpoint".to_string(),
        StopReason::Signal(s) => format!("signal ({s})"),
        StopReason::Exited(code) => format!("exited ({code})"),
        StopReason::Timeout => "timeout — target paused".to_string(),
    };
    let mut out = format!("stopped: {reason}; thread {}; {}\n", report.thread, report.location);
    for frame in &report.frames {
        out.push_str(&format!("  {frame}\n"));
    }
    for (expr, value) in &report.watched {
        out.push_str(&format!("  {expr}: {value}\n"));
    }
    out.trim_end().to_string()
}

fn render_where(stack: &str) -> String {
    stack.lines().take(3).collect::<Vec<_>>().join("\n")
}

/// Flush the target's remaining output to debug.log, then close the session.
async fn flush_output(tb: &Toolbox, mut session: crate::dap::Session) -> Result<String> {
    let output = session.take_output();
    session.quit().await?;
    if output.trim().is_empty() {
        Ok(String::new())
    } else {
        let id = crate::output::log_full(&tb.project, tb.agent, "debug", &output)?;
        Ok(format!(" [target output: log#{}]", id.0))
    }
}

fn absolutize(tb: &Toolbox, path: &str) -> String {
    let p = std::path::Path::new(path);
    if p.is_absolute() { path.to_string() } else { tb.project.join(p).display().to_string() }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tools::testutil;

    fn have_gdb_dap() -> bool {
        let Ok(out) = std::process::Command::new("gdb").arg("--version").output() else {
            return false;
        };
        let banner = String::from_utf8_lossy(&out.stdout);
        // "GNU gdb (…) 17.2" — DAP needs ≥ 14.
        banner
            .split_whitespace()
            .filter_map(|w| w.split('.').next()?.parse::<u32>().ok())
            .any(|major| major >= 14)
    }

    fn compile_debuggee(dir: &std::path::Path) -> bool {
        std::fs::write(
            dir.join("t.c"),
            "#include <stdio.h>\n\
             int add(int a, int b) { int s = a + b; return s; }\n\
             int main() {\n\
                 int t = 0;\n\
                 for (int i = 1; i <= 3; i++) { t = add(t, i); }\n\
                 printf(\"%d\\n\", t);\n\
                 return 0;\n\
             }\n",
        )
        .unwrap();
        std::process::Command::new("gcc")
            .args(["-g", "-O0", "-o"])
            .arg(dir.join("t"))
            .arg(dir.join("t.c"))
            .status()
            .map(|s| s.success())
            .unwrap_or(false)
    }

    /// The full lifecycle through the tool layer: launch → breakpoint →
    /// continue → inspect → run to exit → quit.
    #[tokio::test]
    async fn breakpoint_inspect_and_run_to_exit_against_real_gdb() {
        if !have_gdb_dap() {
            eprintln!("skipping: gdb >= 14 not available");
            return;
        }
        let dir = testutil::tmp("dap-e2e");
        if !compile_debuggee(&dir) {
            eprintln!("skipping: gcc not available");
            return;
        }
        let (mut tb, _ui, _rx) = testutil::toolbox(&dir);

        let out = run(
            &mut tb,
            &serde_json::json!({"actions": [
                {"action": "launch", "program": "t"},
                {"action": "break", "location": "add"},
                {"action": "continue"},
                {"action": "stack", "max_frames": 4},
                {"action": "locals"},
                {"action": "eval", "expr": "a + b"},
                {"action": "command", "text": "info breakpoints"}
            ]}),
        )
        .await
        .unwrap();

        assert!(out.contains("stopped at main"), "launch: {out}");
        assert!(out.contains("breakpoint on add (verified: true)"), "break: {out}");
        assert!(out.contains("stopped: breakpoint"), "continue: {out}");
        assert!(out.contains("add at t.c:2"), "frame: {out}");
        assert!(out.contains("main at t.c:5"), "caller frame: {out}");
        assert!(out.contains("s = "), "locals include the local: {out}");
        assert!(out.contains("── eval a + b\n1"), "eval a+b at first hit: {out}");

        // Run to completion: two more breakpoint hits, then exit 0.
        let out2 = run(
            &mut tb,
            &serde_json::json!({"actions": [
                {"action": "continue"}, {"action": "continue"}, {"action": "continue"},
                {"action": "quit"}
            ]}),
        )
        .await
        .unwrap();
        assert!(out2.contains("exited (0)"), "exit: {out2}");
        assert!(out2.contains("session closed"), "quit: {out2}");
        assert!(tb.debugger.is_none());
    }
}
