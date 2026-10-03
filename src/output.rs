//! Error-aware truncation, secret redaction, and the full-output log — the
//! machinery behind the minimal output contracts (§4, §6.3).

use anyhow::Result;
use std::io::Write;
use std::path::Path;

/// Failure markers prioritized inside the line budget (§6.3).
pub const ERROR_MARKERS: &[&str] = &[
    "error[", "error:", "FAILED", "panicked at", "Traceback",
    "Segmentation fault", "AddressSanitizer",
];

/// Secret-shaped names: a token `NAME=…` with one of these suffixes is scrubbed.
const SECRET_NAME_SUFFIXES: &[&str] = &["KEY", "TOKEN", "SECRET", "PASSWORD", "PASSWD"];

/// Identifies one full output in `debug.log` (its byte offset); rendered as
/// `[full: log#4821]` and addressable by `log_search`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LogId(pub u64);

/// Keep head + tail within `budget_lines`, but always include the regions
/// around error markers — a rustc error mid-stream survives (§6.3). Marker
/// regions may stretch the result up to ~2x the budget; gaps are elision
/// markers so the model knows lines are missing.
pub fn truncate(raw: &str, budget_lines: usize) -> String {
    let lines: Vec<&str> = raw.lines().collect();
    let budget = budget_lines.max(6);
    if lines.len() <= budget {
        return raw.trim_end().to_string();
    }

    let head = (budget / 5).max(2);
    let tail = (budget * 2 / 5).max(3);
    let mut keep = std::collections::BTreeSet::new();
    for i in 0..head.min(lines.len()) {
        keep.insert(i);
    }
    for i in lines.len().saturating_sub(tail)..lines.len() {
        keep.insert(i);
    }
    for (i, line) in lines.iter().enumerate() {
        if keep.len() >= budget * 2 {
            break;
        }
        if ERROR_MARKERS.iter().any(|m| line.contains(m)) {
            for j in i.saturating_sub(1)..=(i + 1).min(lines.len() - 1) {
                keep.insert(j);
            }
        }
    }

    let mut out = String::new();
    let mut last: Option<usize> = None;
    for &i in &keep {
        if let Some(prev) = last {
            if i > prev + 1 {
                out.push_str(&format!("· · · ({} lines elided)\n", i - prev - 1));
            }
        }
        out.push_str(lines[i]);
        out.push('\n');
        last = Some(i);
    }
    out.trim_end().to_string()
}

/// Scrub key/token/bearer shapes before anything hits disk or transcript.
/// Token-level: `NAME=value` with a secret-shaped NAME, the token after
/// `Bearer`, and bare `sk-…` keys. Space-normalizing is acceptable in logs.
pub fn redact(line: &str) -> String {
    let mut out: Vec<String> = Vec::new();
    let mut redact_next = false;
    for tok in line.split(' ') {
        if redact_next && !tok.is_empty() {
            out.push("[redacted]".to_string());
            redact_next = false;
            continue;
        }
        if tok.eq_ignore_ascii_case("bearer") {
            out.push(tok.to_string());
            redact_next = true;
            continue;
        }
        if let Some((name, _)) = tok.split_once('=') {
            let upper = name.to_ascii_uppercase();
            if SECRET_NAME_SUFFIXES.iter().any(|s| upper.ends_with(s)) {
                out.push(format!("{name}=[redacted]"));
                continue;
            }
        }
        if tok.starts_with("sk-") && tok.len() > 12 {
            out.push("[redacted]".to_string());
            continue;
        }
        out.push(tok.to_string());
    }
    out.join(" ")
}

/// Append full raw output to `.tursi/debug.log` (redacted line by line),
/// keyed by agent id per §5.7; returns the id `log_search` retrieves by.
pub fn log_full(project: &Path, agent: crate::bus::AgentId, tool: &str, raw: &str) -> Result<LogId> {
    let dir = project.join(".tursi");
    std::fs::create_dir_all(&dir)?;
    let path = dir.join("debug.log");
    let mut file = std::fs::OpenOptions::new().create(true).append(true).open(&path)?;
    let id = file.metadata()?.len();
    writeln!(file, "── log#{id} agent={} tool={tool} ──", agent.0)?;
    for line in raw.lines() {
        writeln!(file, "{}", redact(line))?;
    }
    Ok(LogId(id))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rust_error_in_middle_of_500_lines_survives_truncation() {
        let mut lines: Vec<String> = (0..500).map(|i| format!("   Compiling dep-{i}")).collect();
        lines[250] = "error[E0308]: mismatched types".to_string();
        lines[251] = "  --> src/auth.rs:45:9".to_string();
        let out = truncate(&lines.join("\n"), 20);
        assert!(out.contains("error[E0308]"));
        assert!(out.contains("src/auth.rs:45:9"));
        assert!(out.lines().count() <= 45, "bounded to ~2x budget");
        assert!(out.contains("lines elided"));
    }

    #[test]
    fn short_output_passes_through_untouched() {
        assert_eq!(truncate("ok\ndone", 20), "ok\ndone");
    }

    #[test]
    fn bearer_tokens_and_key_shapes_never_reach_disk() {
        let r = redact("Authorization: Bearer abc123def456ghi");
        assert!(!r.contains("abc123def456ghi"));
        let r = redact("export DEEPSEEK_API_KEY=sk-abcdef1234567890");
        assert!(!r.contains("sk-abcdef1234567890"));
        let r = redact("curl -H x sk-abcdef1234567890xyz");
        assert!(!r.contains("sk-abcdef1234567890xyz"));
        assert_eq!(redact("cargo build --release"), "cargo build --release");
    }

    #[test]
    fn log_full_returns_addressable_increasing_ids() {
        let dir = std::env::temp_dir().join(format!("tursi-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let a = log_full(&dir, crate::bus::ROOT, "execute_command", "one\ntwo").unwrap();
        let b = log_full(&dir, crate::bus::ROOT, "profile", "three").unwrap();
        assert!(b.0 > a.0);
        let log = std::fs::read_to_string(dir.join(".tursi/debug.log")).unwrap();
        assert!(log.contains(&format!("log#{}", a.0)));
        assert!(log.contains(&format!("log#{}", b.0)));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
