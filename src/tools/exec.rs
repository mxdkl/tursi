//! execute_command: stepped execution in one shell (§4.1, §6.1). Owns the
//! policy layer — permission checks, approval, stream selection, truncation,
//! logging; the sandbox is mechanism.

use anyhow::{Result, bail};
use serde::Deserialize;
use serde_json::Value;
use std::time::Duration;
use tokio::sync::oneshot;

use crate::bus::{ApprovalReply, ApprovalRequest, EventKind, UiHandle};
use crate::output;
use crate::sandbox::{OnError, Step, StepResult, Streams};
use crate::shell;
use crate::tools::Toolbox;

#[derive(Deserialize)]
pub struct ExecArgs {
    pub steps: Vec<StepArg>,
    pub on_error: Option<String>,
    /// Run the steps in the background: the call returns at once with a
    /// monitor id, and the exit (with an output tail) wakes the model.
    pub background: Option<bool>,
    /// Name for the background job in wake messages.
    pub label: Option<String>,
}

#[derive(Deserialize)]
pub struct StepArg {
    pub command: String,
    /// Run this step here (relative to project root) — replaces `cd`.
    pub cwd: Option<String>,
    /// Env vars for this step, e.g. {"LD_PRELOAD": "./x.so"} — replaces `export`.
    pub env: Option<std::collections::BTreeMap<String, String>>,
    /// auto | none | stdout | stderr | both — exit code always comes back (§6.1).
    pub streams: Option<String>,
    pub timeout_seconds: Option<u64>,
    pub tail_lines: Option<usize>,
    /// `"full"`: this call needs more than the package registries — asks the
    /// user; the grant lasts for this call only (PERMISSIONS.md §4.3).
    pub network: Option<String>,
    /// Models put call-level flags on steps too; honored as the call's.
    pub background: Option<bool>,
    pub label: Option<String>,
}

/// Env var names must be plain identifiers: they go into `export <KEY>=…`
/// unquoted, so anything else could break out of the assignment.
fn valid_env_key(k: &str) -> bool {
    !k.is_empty()
        && !k.starts_with(|c: char| c.is_ascii_digit())
        && k.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
}

/// Parse → validate (chaining ban) → the network prompt if a step asks for
/// it (PERMISSIONS.md §4.3) → run in one shell → render one line per step.
pub async fn run(tb: &mut Toolbox, args: &Value, ui: &UiHandle) -> Result<String> {
    let args: ExecArgs = serde_json::from_value(args.clone())?;
    if args.steps.is_empty() {
        bail!("no steps given");
    }
    for step in &args.steps {
        shell::validate_step(&step.command, tb.sandbox.bash)?;
    }
    let full_network = args.steps.iter().any(|s| s.network.as_deref() == Some("full"));
    for step in &args.steps {
        if let Some(other) = step.network.as_deref().filter(|n| *n != "full") {
            bail!("unknown network value {other:?} — only \"full\" is accepted");
        }
    }
    let background = args.background.unwrap_or(false) || args.steps.iter().any(|s| s.background.unwrap_or(false));
    if background && full_network {
        bail!("background calls can't take `network: \"full\"` — the grant lasts one foreground call");
    }
    if full_network {
        if let Err(reason) = approve_network(tb, &args, ui).await? {
            bail!(
                "full network refused ({reason}) — the package registries stay reachable without the flag; \
                 rerun without `network` if that suffices"
            );
        }
    }
    let steps = to_steps(tb, &args)?;
    if background {
        return self::background(tb, &args, &steps, ui).await;
    }
    tb.sandbox.grant_network(full_network);
    let results = tb.sandbox.run_steps(steps.clone(), parse_on_error(&args)?).await;
    tb.sandbox.grant_network(false);
    let mut out = render(tb, &steps, &results?)?;
    let blocked = tb.sandbox.blocked_hosts();
    if !blocked.is_empty() {
        out.push_str(&format!(
            "\n[network] blocked: {} — only package registries are reachable; if this access is needed, \
             add `network: \"full\"` to the step and rerun (the user is asked)",
            blocked.join(", ")
        ));
    }
    Ok(out)
}

/// A background call: the steps as one script (each step's `cwd`/`env` in a
/// subshell, like the live shell; `on_error` as `exit` after a failing step),
/// run under a quiet monitor. Keep working; the exit wakes you.
async fn background(tb: &mut Toolbox, args: &ExecArgs, steps: &[Step], ui: &UiHandle) -> Result<String> {
    let stop_on_error = parse_on_error(args)? == OnError::Stop;
    let mut script = String::new();
    for step in steps {
        let body = if step.cwd.is_some() || !step.env.is_empty() {
            let mut pre = String::new();
            if let Some(cwd) = &step.cwd {
                pre.push_str(&format!("cd {} || exit $?\n", crate::sandbox::sh_quote(&cwd.display().to_string())));
            }
            for (k, v) in &step.env {
                pre.push_str(&format!("export {k}={}\n", crate::sandbox::sh_quote(v)));
            }
            format!("(\n{pre}{}\n)", step.command)
        } else {
            step.command.clone()
        };
        script.push_str(&body);
        script.push('\n');
        if stop_on_error {
            script.push_str("__s=$?; [ \"$__s\" -eq 0 ] || exit \"$__s\"\n");
        }
    }
    let label = args
        .label
        .clone()
        .or_else(|| args.steps.iter().find_map(|s| s.label.clone()))
        .unwrap_or_else(|| steps.first().map(|s| s.command.chars().take(40).collect()).unwrap_or_default());
    let name = format!("bg-{}.sh", uuid::Uuid::now_v7().simple());
    let path = tb.sandbox.scratch_file(&name, &script)?;
    let timeout = Some(steps.iter().map(|s| s.timeout).sum::<Duration>().max(Duration::from_secs(60)));
    let id = tb
        .monitors
        .watch_command(format!("{} {}", tb.sandbox.shell, crate::sandbox::sh_quote(&path.display().to_string())), label.clone(), timeout, true)
        .await?;
    ui.send(EventKind::Monitors { armed: tb.monitors.list().into_iter().map(|(id, label, _)| (id, label)).collect() }).await;
    Ok(format!(
        "{id} running in the background: {label} ({} step(s)) — keep working, or end your turn; its exit and output will reach you by themselves (no monitor needed). `monitor` {{stop:\"{id}\"}} kills it.",
        steps.len()
    ))
}

/// The one permission prompt that survives the sandbox (PERMISSIONS.md §1.1):
/// full network for this call. Nobody there (AFK/headless) → refused.
async fn approve_network(tb: &Toolbox, args: &ExecArgs, ui: &UiHandle) -> Result<std::result::Result<(), String>> {
    if !tb.sandbox.sandboxed() {
        return Ok(Ok(()));
    }
    if tb.afk {
        return Ok(Err("no one is here to approve it".to_string()));
    }
    let listing: Vec<String> = args.steps.iter().map(|s| format!("  {}", s.command)).collect();
    let (reply, rx) = oneshot::channel();
    ui.send(EventKind::Approval(ApprovalRequest {
        summary: "allow FULL network access for this call?".to_string(),
        diff: Some(listing.join("\n")),
        warn: true,
        reply,
    }))
    .await;
    match rx.await? {
        ApprovalReply::Approve => Ok(Ok(())),
        ApprovalReply::Reject { reason } => Ok(Err(reason.unwrap_or_else(|| "declined".to_string()))),
    }
}

/// Apply defaults and the config timeout cap.
fn to_steps(tb: &Toolbox, args: &ExecArgs) -> Result<Vec<Step>> {
    args.steps
        .iter()
        .map(|s| {
            let streams = match s.streams.as_deref() {
                None | Some("auto") => Streams::Auto,
                Some("none") => Streams::None,
                Some("stdout") => Streams::Stdout,
                Some("stderr") => Streams::Stderr,
                Some("both") => Streams::Both,
                Some(other) => bail!("unknown streams value: {other}"),
            };
            let env: Vec<(String, String)> = match &s.env {
                Some(map) => {
                    for k in map.keys() {
                        if !valid_env_key(k) {
                            bail!("invalid env var name {k:?} — use letters, digits, underscore");
                        }
                    }
                    map.iter().map(|(k, v)| (k.clone(), v.clone())).collect()
                }
                None => Vec::new(),
            };
            // cwd is relative to the project root, anchored to an absolute path
            // so it can't be thrown off by a stray persistent `cd`.
            let cwd = s.cwd.as_ref().map(|c| tb.project.join(c));
            Ok(Step {
                command: s.command.clone(),
                cwd,
                env,
                streams,
                timeout: Duration::from_secs(
                    s.timeout_seconds.unwrap_or(30).min(tb.sandbox.timeout_cap.as_secs()),
                ),
                tail_lines: s.tail_lines.unwrap_or(20),
            })
        })
        .collect()
}

fn parse_on_error(args: &ExecArgs) -> Result<OnError> {
    match args.on_error.as_deref() {
        None | Some("stop") => Ok(OnError::Stop),
        Some("continue") => Ok(OnError::Continue),
        Some(other) => bail!("unknown on_error value: {other}"),
    }
}

/// `1 ✓ cargo build (4.1s)` / `2 ✗ … [full: log#N]` / `3 – not run` (§6.1),
/// stream bodies indented beneath their status line per the step's setting.
fn render(tb: &Toolbox, steps: &[Step], results: &[StepResult]) -> Result<String> {
    let mut out = String::new();
    for (i, (step, r)) in steps.iter().zip(results).enumerate() {
        let n = i + 1;
        if !r.ran {
            out.push_str(&format!("{n} – {}    not run\n", r.command));
            continue;
        }
        let ok = r.exit_code == Some(0);
        let secs = r.elapsed.as_secs_f32();
        let status = match r.exit_code {
            Some(0) => format!("({secs:.1}s)"),
            // Say which stage failed: `cargo test | tail` exits 101 with the
            // last stage's output.
            Some(code) if r.pipe_status.len() > 1 => {
                let stages: Vec<String> = r.pipe_status.iter().map(i32::to_string).collect();
                format!("exit {code} (pipeline {}) ({secs:.1}s)", stages.join("|"))
            }
            Some(code) => format!("exit {code} ({secs:.1}s)"),
            None => format!("killed after {secs:.1}s (timeout or shell death)"),
        };
        let log_ref = if r.stdout.is_empty() && r.stderr.is_empty() {
            String::new()
        } else {
            let full = format!("stdout:\n{}\nstderr:\n{}", r.stdout, r.stderr);
            let id = output::log_full(&tb.project, tb.agent, "execute_command", &full)?;
            format!(" [full: log#{}]", id.0)
        };
        let mark = if ok { '✓' } else { '✗' };
        out.push_str(&format!("{n} {mark} {}    {status}{log_ref}\n", r.command));

        let body = match step.streams {
            // Success: show stdout (the point of find/ls/grep/cat); empty for
            // build/test-style commands, so those still collapse to one line.
            Streams::Auto if ok => output::truncate(&r.stdout, step.tail_lines),
            Streams::Auto => output::truncate(&format!("{}\n{}", r.stderr, r.stdout), step.tail_lines),
            Streams::None => String::new(),
            Streams::Stdout => output::truncate(&r.stdout, step.tail_lines),
            Streams::Stderr => output::truncate(&r.stderr, step.tail_lines),
            Streams::Both => output::truncate(&format!("{}\n{}", r.stdout, r.stderr), step.tail_lines),
        };
        if !body.trim().is_empty() {
            for line in body.lines() {
                out.push_str("  ");
                out.push_str(line);
                out.push('\n');
            }
        }
    }
    Ok(out.trim_end().to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tools::testutil;

    #[tokio::test]
    async fn steps_pipeline_end_to_end() {
        let dir = testutil::tmp("exec-e2e");
        let (mut tb, ui, _rx) = testutil::toolbox(&dir);
        let out = run(
            &mut tb,
            &serde_json::json!({"steps": [
                {"command": "echo hi | tr i o", "streams": "stdout"},
                {"command": "false"},
                {"command": "echo never"}
            ]}),
            &ui,
        )
        .await
        .unwrap();
        assert!(out.contains("1 ✓"), "got: {out}");
        assert!(out.contains("ho"));
        assert!(out.contains("2 ✗"));
        assert!(out.contains("exit 1"));
        assert!(out.contains("3 – echo never    not run"));
        assert!(out.contains("[full: log#"));
        let log = std::fs::read_to_string(dir.join(".tursi/debug.log")).unwrap();
        assert!(log.contains("execute_command"));
    }

    #[tokio::test]
    async fn auto_shows_stdout_on_success_but_stays_quiet_when_empty() {
        let dir = testutil::tmp("exec-auto");
        let (mut tb, ui, _rx) = testutil::toolbox(&dir);
        // find/ls/grep: their stdout is the point — auto must show it.
        let listing = run(
            &mut tb,
            &serde_json::json!({"steps": [{"command": "printf 'a.rs\\nb.rs\\n'"}]}),
            &ui,
        )
        .await
        .unwrap();
        assert!(listing.contains("a.rs") && listing.contains("b.rs"), "auto hid stdout: {listing}");
        // A command with no stdout still collapses to one status line.
        let quiet = run(
            &mut tb,
            &serde_json::json!({"steps": [{"command": "true"}]}),
            &ui,
        )
        .await
        .unwrap();
        assert_eq!(quiet.lines().count(), 1, "empty-stdout success should be one line: {quiet}");
    }

    #[tokio::test]
    async fn cwd_and_env_run_the_step_in_context() {
        let dir = testutil::tmp("exec-cwdenv");
        std::fs::create_dir_all(dir.join("sub")).unwrap();
        std::fs::write(dir.join("sub/here.txt"), "").unwrap();
        let (mut tb, ui, _rx) = testutil::toolbox(&dir);
        let out = run(
            &mut tb,
            &serde_json::json!({"steps": [
                {"command": "ls", "cwd": "sub", "streams": "stdout"},
                {"command": "echo $LD_PRELOAD", "env": {"LD_PRELOAD": "libx.so"}, "streams": "stdout"}
            ]}),
            &ui,
        )
        .await
        .unwrap();
        assert!(out.contains("here.txt"), "cwd should run ls in sub/: {out}");
        assert!(out.contains("libx.so"), "env should reach the step: {out}");
    }

    #[tokio::test]
    async fn invalid_env_key_is_rejected() {
        let dir = testutil::tmp("exec-badenv");
        let (mut tb, ui, _rx) = testutil::toolbox(&dir);
        let err = run(
            &mut tb,
            &serde_json::json!({"steps": [{"command": "true", "env": {"BAD KEY": "x"}}]}),
            &ui,
        )
        .await
        .unwrap_err();
        assert!(err.to_string().contains("invalid env var name"), "{err}");
    }

    #[tokio::test]
    async fn full_network_is_refused_when_no_one_can_approve() {
        let dir = testutil::tmp("exec-net-afk");
        let (mut tb, ui, _rx) = testutil::toolbox(&dir);
        if !tb.sandbox.sandboxed() {
            eprintln!("skipping: unsandboxed test run");
            return;
        }
        tb.afk = true;
        let err = run(
            &mut tb,
            &serde_json::json!({"steps": [{"command": "true", "network": "full"}]}),
            &ui,
        )
        .await
        .unwrap_err()
        .to_string();
        assert!(err.contains("full network refused") && err.contains("registries"), "{err}");
        // Without the flag the call runs, and a refused host is reported.
        let out = run(
            &mut tb,
            &serde_json::json!({"steps": [
                {"command": "exec 3<>/dev/tcp/127.0.0.1/3128"},
                {"command": "printf 'CONNECT x.example:443 HTTP/1.1\\r\\n\\r\\n' >&3"},
                {"command": "head -1 <&3", "streams": "none"}
            ]}),
            &ui,
        )
        .await
        .unwrap();
        assert!(out.contains("[network] blocked: x.example:443"), "{out}");
    }

    #[tokio::test]
    async fn a_background_call_returns_at_once_under_a_quiet_monitor() {
        let dir = testutil::tmp("exec-bg");
        let (mut tb, ui, _rx) = testutil::toolbox(&dir);
        let started = std::time::Instant::now();
        let out = run(
            &mut tb,
            &serde_json::json!({"steps": [{"command": "sleep 2"}, {"command": "touch after.txt"}], "background": true, "label": "slow build"}),
            &ui,
        )
        .await
        .unwrap();
        assert!(started.elapsed() < std::time::Duration::from_secs(1), "must not wait for the steps");
        assert!(out.starts_with("m1 running in the background: slow build (2 step(s))"), "{out}");
        assert_eq!(tb.monitors.list(), vec![("m1".to_string(), "slow build".to_string(), "background".to_string())]);
        // The steps really run: the second one lands after the first finishes.
        assert!(!dir.join("after.txt").exists());
        tokio::time::sleep(std::time::Duration::from_millis(2600)).await;
        assert!(dir.join("after.txt").exists(), "background steps ran to completion");
        // Network grants are per foreground call: refused for background.
        let err = run(&mut tb, &serde_json::json!({"steps": [{"command": "true", "network": "full"}], "background": true}), &ui).await.unwrap_err();
        assert!(err.to_string().contains("background calls can't"), "{err}");
    }

    #[tokio::test]
    async fn chained_commands_are_rejected_before_anything_runs() {
        let dir = testutil::tmp("exec-chain");
        let (mut tb, ui, _rx) = testutil::toolbox(&dir);
        let err = run(
            &mut tb,
            &serde_json::json!({"steps": [{"command": "echo a && echo b"}]}),
            &ui,
        )
        .await
        .unwrap_err();
        assert!(err.to_string().contains("use separate steps"));
    }
}
