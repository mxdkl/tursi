//! search (ripgrep-backed) and log_search (§4.1) — read-only. ripgrep runs
//! inside the sandbox, so it sees exactly the model's view; log_search reads
//! the harness's own debug.log.

use anyhow::{Result, bail};
use serde::Deserialize;
use serde_json::Value;
use std::collections::BTreeSet;

use crate::tools::Toolbox;

/// Result bytes per call, whatever `max_results` says (§0.2: narrow, or page).
const MAX_SEARCH_BYTES: usize = 16 * 1024;

#[derive(Deserialize)]
pub struct SearchArgs {
    /// Regex (ripgrep syntax). Omit with `glob` set to just find files.
    pub pattern: Option<String>,
    pub path: Option<String>,
    pub glob: Option<String>,
    pub max_results: Option<usize>,
}

#[derive(Deserialize)]
pub struct LogSearchArgs {
    /// Substring match over the session log.
    pub pattern: String,
    pub context_lines: Option<usize>,
}

/// Structured `file:line:` results grouped by file, hard cap with an
/// "N more matches" line. `glob` with no pattern = file finding (§4.1).
pub async fn search(tb: &Toolbox, args: &Value) -> Result<String> {
    let args: SearchArgs = serde_json::from_value(args.clone())?;
    let max = args.max_results.unwrap_or(50).clamp(1, 500);

    // Harness state is never what the model is looking for.
    let mut argv: Vec<String> = vec!["rg".into(), "--glob".into(), "!.tursi/**".into()];
    let files_only = match (&args.pattern, &args.glob) {
        (None, None) => bail!("give pattern, glob, or both"),
        (None, Some(glob)) => {
            argv.extend(["--files".into(), "--glob".into(), glob.clone()]);
            true
        }
        (Some(pattern), glob) => {
            argv.extend(["--line-number", "--no-heading", "--color", "never", "--max-columns", "200"].map(String::from));
            if let Some(glob) = glob {
                argv.extend(["--glob".into(), glob.clone()]);
            }
            argv.extend(["--".into(), pattern.clone()]);
            false
        }
    };
    if let Some(path) = &args.path {
        argv.push(path.clone());
    }

    let out = tb.sandbox.output(argv, None, std::time::Duration::from_secs(60)).await?;
    let stderr = String::from_utf8_lossy(&out.stderr);
    match out.code {
        Some(0) => {}
        Some(1) => return Ok("no matches".to_string()),
        Some(127) | Some(126) => bail!("ripgrep (rg) is not installed — search needs it"),
        None => bail!("search timed out"),
        _ => bail!("search failed: {}", stderr.trim()),
    }

    let stdout = String::from_utf8_lossy(&out.stdout);
    let lines: Vec<&str> = stdout.lines().collect();
    let total = lines.len();
    let mut rendered = String::new();
    let mut last_file = "";
    let mut shown = 0;
    for line in lines.iter().take(max) {
        if rendered.len() >= MAX_SEARCH_BYTES {
            break;
        }
        shown += 1;
        if files_only {
            rendered.push_str(line);
            rendered.push('\n');
            continue;
        }
        // path:line:content — group under one header per file.
        let mut parts = line.splitn(3, ':');
        let (Some(file), Some(lineno), Some(content)) = (parts.next(), parts.next(), parts.next())
        else {
            continue;
        };
        if file != last_file {
            rendered.push_str(file);
            rendered.push('\n');
            last_file = file;
        }
        rendered.push_str(&format!("  {lineno}: {}\n", content.trim_end()));
    }
    if total > shown {
        rendered.push_str(&format!(
            "… {} more matches — narrow the pattern, add a glob/path, or raise max_results\n",
            total - shown
        ));
    }
    Ok(rendered.trim_end().to_string())
}

/// Substring grep over this session's full debug.log with context windows —
/// the recovery path that makes aggressive truncation safe (§4). `log#N` ids
/// from tool results are directly addressable.
pub async fn log_search(tb: &Toolbox, args: &Value) -> Result<String> {
    let args: LogSearchArgs = serde_json::from_value(args.clone())?;
    let ctx = args.context_lines.unwrap_or(2).min(20);
    let path = tb.project.join(".tursi/debug.log");
    let content = std::fs::read_to_string(&path).unwrap_or_default();
    if content.is_empty() {
        return Ok("the session log is empty".to_string());
    }

    const MAX_MATCHES: usize = 20;
    let lines: Vec<&str> = content.lines().collect();
    let mut keep = BTreeSet::new();
    let mut matches = 0usize;
    let mut more = false;
    for (i, line) in lines.iter().enumerate() {
        if line.contains(&args.pattern) {
            matches += 1;
            if matches > MAX_MATCHES {
                more = true;
                break;
            }
            for j in i.saturating_sub(ctx)..=(i + ctx).min(lines.len() - 1) {
                keep.insert(j);
            }
        }
    }
    if keep.is_empty() {
        return Ok("no matches in the session log".to_string());
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
    if more {
        out.push_str("… more matches exist — refine the pattern\n");
    }
    Ok(out.trim_end().to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tools::testutil;

    fn have_rg() -> bool {
        std::process::Command::new("rg").arg("--version").output().is_ok()
    }

    #[tokio::test]
    async fn search_groups_by_file_and_caps_results() {
        if !have_rg() {
            eprintln!("skipping: rg not installed");
            return;
        }
        let dir = testutil::tmp("search");
        std::fs::write(dir.join("a.rs"), "needle one\nneedle two\n").unwrap();
        std::fs::write(dir.join("b.rs"), "needle three\n").unwrap();
        let (tb, _ui, _rx) = testutil::toolbox(&dir);
        let out = search(&tb, &serde_json::json!({"pattern": "needle"})).await.unwrap();
        assert!(out.contains("a.rs"));
        assert!(out.contains("b.rs"));
        assert!(out.contains("1: needle"));
        let capped = search(&tb, &serde_json::json!({"pattern": "needle", "max_results": 2}))
            .await
            .unwrap();
        assert!(capped.contains("1 more matches"), "got: {capped}");
        let files = search(&tb, &serde_json::json!({"glob": "*.rs"})).await.unwrap();
        assert!(files.contains("a.rs") && files.contains("b.rs"));
        let none = search(&tb, &serde_json::json!({"pattern": "absent_xyz"})).await.unwrap();
        assert_eq!(none, "no matches");
    }

    #[tokio::test]
    async fn log_search_returns_context_windows_by_log_id() {
        let dir = testutil::tmp("logsearch");
        let (tb, _ui, _rx) = testutil::toolbox(&dir);
        let id = crate::output::log_full(&dir, crate::bus::ROOT, "execute_command", "alpha\nbeta\ngamma\ndelta\nepsilon").unwrap();
        let out = log_search(&tb, &serde_json::json!({"pattern": format!("log#{}", id.0), "context_lines": 1}))
            .await
            .unwrap();
        assert!(out.contains(&format!("log#{}", id.0)));
        assert!(out.contains("alpha"), "context line included: {out}");
    }
}
