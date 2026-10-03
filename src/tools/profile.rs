//! profile: five modes, compressed output, raw artifacts to .tursi/profiles/
//! (§4.2). The harness owns the flag soup; commands run in the sandbox like
//! execute_command's.

use anyhow::{Result, bail};
use serde::Deserialize;
use serde_json::Value;
use std::time::Duration;

use crate::output;
use crate::sandbox::{OnError, Step, StepResult, Streams};
use crate::shell;
use crate::tools::Toolbox;

#[derive(Deserialize)]
pub struct ProfileArgs {
    pub command: String,
    pub mode: Mode,
    pub runs: Option<u32>,
    /// A/B comparison command for `time` mode.
    pub baseline: Option<String>,
}

#[derive(Debug, Clone, Copy, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Mode {
    Time,
    Counters,
    Hotspots,
    Allocs,
    Syscalls,
}

pub async fn run(tb: &mut Toolbox, args: &Value) -> Result<String> {
    let args: ProfileArgs = serde_json::from_value(args.clone())?;
    shell::validate_step(&args.command, tb.sandbox.bash)?;
    if let Some(baseline) = &args.baseline {
        shell::validate_step(baseline, tb.sandbox.bash)?;
    }

    match args.mode {
        Mode::Time => time(tb, &args).await,
        Mode::Counters => counters(tb, &args).await,
        Mode::Hotspots => hotspots(tb, &args).await,
        Mode::Allocs => allocs(tb, &args).await,
        Mode::Syscalls => syscalls(tb, &args).await,
    }
}

/// Warmup + N timed runs in ONE shell (no per-run spawn overhead); mean ± σ;
/// with `baseline`, the relative delta. Warns when the CPU governor would
/// make deltas lie.
async fn time(tb: &mut Toolbox, args: &ProfileArgs) -> Result<String> {
    let runs = args.runs.unwrap_or(5).clamp(1, 50) as usize;
    let series_a = series(tb, &args.command, runs).await?;
    let (mean_a, sd_a, min_a, max_a) = stats(&series_a);

    let mut out = format!(
        "profile(time) {}\n  mean {:.1}ms ± {:.1}ms  (min {:.1}, max {:.1}, n={runs})\n",
        args.command, mean_a, sd_a, min_a, max_a
    );
    if let Some(baseline) = &args.baseline {
        let series_b = series(tb, baseline, runs).await?;
        let (mean_b, sd_b, ..) = stats(&series_b);
        out.push_str(&format!("baseline {baseline}\n  mean {mean_b:.1}ms ± {sd_b:.1}ms\n"));
        let delta = if mean_a <= mean_b {
            format!("{:.2}x faster than baseline", mean_b / mean_a.max(0.001))
        } else {
            format!("{:.2}x slower than baseline", mean_a / mean_b.max(0.001))
        };
        out.push_str(&format!("delta: {delta}\n"));
    }
    if let Some(governor) = governor() {
        if governor != "performance" {
            out.push_str(&format!(
                "[warn] cpu governor is '{governor}' — timings are noisy; deltas below ~20% are not trustworthy\n"
            ));
        }
    }
    Ok(out.trim_end().to_string())
}

/// One run_steps call: warmup (discarded) + N repeats; per-step elapsed from
/// the sandbox is the measurement.
async fn series(tb: &mut Toolbox, command: &str, runs: usize) -> Result<Vec<f64>> {
    let step = |c: &str| Step {
        command: c.to_string(),
        cwd: None,
        env: vec![],
        streams: Streams::Auto,
        timeout: tb.sandbox.timeout_cap,
        tail_lines: 20,
    };
    let mut steps = vec![step(command)]; // warmup
    steps.extend(std::iter::repeat_with(|| step(command)).take(runs));
    let results = tb.sandbox.run_steps(steps, OnError::Stop).await?;
    if let Some(red) = results.iter().find(|r| r.ran && r.exit_code != Some(0)) {
        bail!(
            "command failed during timing (exit {:?}): {}",
            red.exit_code,
            output::truncate(&format!("{}\n{}", red.stderr, red.stdout), 10)
        );
    }
    Ok(results.iter().skip(1).map(|r| r.elapsed.as_secs_f64() * 1000.0).collect())
}

fn stats(ms: &[f64]) -> (f64, f64, f64, f64) {
    let n = ms.len().max(1) as f64;
    let mean = ms.iter().sum::<f64>() / n;
    let var = ms.iter().map(|x| (x - mean).powi(2)).sum::<f64>() / n;
    let min = ms.iter().copied().fold(f64::INFINITY, f64::min);
    let max = ms.iter().copied().fold(0.0, f64::max);
    (mean, var.sqrt(), min, max)
}

fn governor() -> Option<String> {
    std::fs::read_to_string("/sys/devices/system/cpu/cpu0/cpufreq/scaling_governor")
        .ok()
        .map(|s| s.trim().to_string())
}

const PERF_EVENTS: &str =
    "cycles,instructions,branches,branch-misses,cache-references,cache-misses,page-faults,context-switches";

async fn counters(tb: &mut Toolbox, args: &ProfileArgs) -> Result<String> {
    let command = format!("perf stat -x, -e {PERF_EVENTS} -- {} -c {}", tb.sandbox.shell, shq(&args.command));
    let result = one(tb, &command, 120).await?;
    if result.exit_code == Some(127) {
        bail!("perf is not installed — counters mode needs it");
    }
    let full = format!("stdout:\n{}\nstderr:\n{}", result.stdout, result.stderr);
    let id = output::log_full(&tb.project, tb.agent, "profile", &full)?;

    // perf -x, CSV on stderr: value,unit,event,…
    let mut values = std::collections::HashMap::new();
    for line in result.stderr.lines() {
        let mut fields = line.split(',');
        let (Some(value), Some(_unit), Some(event)) = (fields.next(), fields.next(), fields.next())
        else {
            continue;
        };
        if let Ok(v) = value.trim().parse::<f64>() {
            values.insert(event.trim().to_string(), v);
        }
    }
    if values.is_empty() {
        bail!(
            "perf produced no counters (paranoid setting?): {} [full: log#{}]",
            output::truncate(&result.stderr, 10),
            id.0
        );
    }

    let mut out = format!("profile(counters) {} [full: log#{}]\n", args.command, id.0);
    for event in PERF_EVENTS.split(',') {
        if let Some(v) = values.get(event) {
            out.push_str(&format!("  {event:<18} {}\n", commafy(*v)));
        }
    }
    if let (Some(i), Some(c)) = (values.get("instructions"), values.get("cycles")) {
        out.push_str(&format!("  IPC                {:.2}\n", i / c.max(1.0)));
    }
    if let (Some(m), Some(b)) = (values.get("branch-misses"), values.get("branches")) {
        out.push_str(&format!("  branch-miss rate   {:.2}%\n", 100.0 * m / b.max(1.0)));
    }
    if let (Some(m), Some(r)) = (values.get("cache-misses"), values.get("cache-references")) {
        out.push_str(&format!("  cache-miss rate    {:.2}%\n", 100.0 * m / r.max(1.0)));
    }
    Ok(out.trim_end().to_string())
}

async fn hotspots(tb: &mut Toolbox, args: &ProfileArgs) -> Result<String> {
    let dir = tb.project.join(".tursi/profiles").join(uuid::Uuid::now_v7().simple().to_string());
    std::fs::create_dir_all(&dir)?;
    let data = dir.join("perf.data");
    let record = format!(
        "perf record -g --output '{}' -- {} -c {}",
        data.display(),
        tb.sandbox.shell,
        shq(&args.command)
    );
    let report = format!(
        "perf report --stdio --no-children --percent-limit 0.5 --input '{}'",
        data.display()
    );
    let steps = vec![
        Step { command: record, cwd: None, env: vec![], streams: Streams::Auto, timeout: tb.sandbox.timeout_cap, tail_lines: 20 },
        Step { command: report, cwd: None, env: vec![], streams: Streams::Stdout, timeout: Duration::from_secs(60), tail_lines: 400 },
    ];
    let results = tb.sandbox.run_steps(steps, OnError::Stop).await?;
    if results[0].exit_code == Some(127) {
        bail!("perf is not installed — hotspots mode needs it");
    }
    if results[0].exit_code != Some(0) {
        bail!(
            "perf record failed: {}",
            output::truncate(&format!("{}\n{}", results[0].stderr, results[0].stdout), 10)
        );
    }
    let top: Vec<&str> = results[1]
        .stdout
        .lines()
        .filter(|l| l.trim_start().starts_with(|c: char| c.is_ascii_digit()) && l.contains('%'))
        .take(15)
        .collect();
    let mut out = format!("profile(hotspots) {}\n", args.command);
    for line in &top {
        out.push_str(&format!("  {}\n", line.trim()));
    }
    if top.is_empty() {
        out.push_str("  (no samples above 0.5% — too short? try a longer run)\n");
    }
    out.push_str(&format!(
        "raw profile: {} — flamegraph: perf script -i <that> | inferno",
        data.display()
    ));
    Ok(out)
}

/// v0: peak RSS + faults via GNU time; heaptrack allocation-site integration
/// is a later increment.
async fn allocs(tb: &mut Toolbox, args: &ProfileArgs) -> Result<String> {
    let command = format!("/usr/bin/time -v {} -c {}", tb.sandbox.shell, shq(&args.command));
    let result = one(tb, &command, 300).await?;
    if result.exit_code == Some(127) {
        bail!("allocs mode needs GNU time (/usr/bin/time) or heaptrack — neither is installed");
    }
    let mut out = format!("profile(allocs) {}\n", args.command);
    for line in result.stderr.lines() {
        let l = line.trim();
        if l.starts_with("Maximum resident set size")
            || l.starts_with("Minor (reclaiming")
            || l.starts_with("Major (requiring")
        {
            out.push_str(&format!("  {l}\n"));
        }
    }
    out.push_str("  (allocation sites need heaptrack — pending)");
    Ok(out)
}

async fn syscalls(tb: &mut Toolbox, args: &ProfileArgs) -> Result<String> {
    let command = format!("strace -c -f -- {} -c {}", tb.sandbox.shell, shq(&args.command));
    let result = one(tb, &command, 300).await?;
    if result.exit_code == Some(127) {
        bail!("strace is not installed — syscalls mode needs it");
    }
    let full = format!("stdout:\n{}\nstderr:\n{}", result.stdout, result.stderr);
    let id = output::log_full(&tb.project, tb.agent, "profile", &full)?;
    Ok(format!(
        "profile(syscalls) {} [full: log#{}]\n{}",
        args.command,
        id.0,
        output::truncate(&result.stderr, 30)
    ))
}

async fn one(tb: &Toolbox, command: &str, timeout_secs: u64) -> Result<StepResult> {
    let step = Step {
        command: command.to_string(),
        cwd: None,
        env: vec![],
        streams: Streams::Both,
        timeout: Duration::from_secs(timeout_secs).min(tb.sandbox.timeout_cap),
        tail_lines: 60,
    };
    let mut results = tb.sandbox.run_steps(vec![step], OnError::Stop).await?;
    Ok(results.pop().expect("one step in, one result out"))
}

/// Single-quote a string for embedding in `sh -c`.
fn shq(s: &str) -> String {
    format!("'{}'", s.replace('\'', r#"'\''"#))
}

fn commafy(v: f64) -> String {
    let n = v as u64;
    let s = n.to_string();
    let mut out = String::new();
    for (i, c) in s.chars().enumerate() {
        if i > 0 && (s.len() - i) % 3 == 0 {
            out.push(',');
        }
        out.push(c);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tools::testutil;

    #[tokio::test]
    async fn time_mode_reports_mean_sigma_and_baseline_delta() {
        let dir = testutil::tmp("prof-time");
        let (mut tb, _ui, _rx) = testutil::toolbox(&dir);
        let out = run(
            &mut tb,
            &serde_json::json!({"command": "true", "mode": "time", "runs": 3, "baseline": "sleep 0.05"}),
        )
        .await
        .unwrap();
        assert!(out.contains("mean"), "got: {out}");
        assert!(out.contains("n=3"));
        assert!(out.contains("faster than baseline"), "true beats sleep: {out}");
    }

    #[tokio::test]
    async fn syscalls_mode_returns_strace_summary_when_available() {
        if std::process::Command::new("strace").arg("-V").output().is_err() {
            eprintln!("skipping: strace not installed");
            return;
        }
        let dir = testutil::tmp("prof-sys");
        let (mut tb, _ui, _rx) = testutil::toolbox(&dir);
        let out = run(
            &mut tb,
            &serde_json::json!({"command": "true", "mode": "syscalls"}),
        )
        .await
        .unwrap();
        assert!(out.contains("profile(syscalls)"));
        assert!(out.to_lowercase().contains("syscall") || out.contains("execve"), "got: {out}");
    }
}
