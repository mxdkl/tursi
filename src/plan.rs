//! Skeleton plan mode (§5.5): stubs as the plan, the typechecker as the gate.

use anyhow::Result;
use std::path::{Path, PathBuf};

use crate::output;
use crate::sandbox::{OnError, Sandbox, Step, Streams};

/// Stub markers per language. Counting is literal-aware: a marker mentioned
/// inside a string or comment is not a stub (tursi's own source proved the
/// naive count wrong — STUB_MARKERS below would count itself).
pub const STUB_MARKERS: &[&str] = &[
    "todo!()",
    "raise NotImplementedError",
    "throw new Error(\"TODO\")",
];

const SOURCE_EXTENSIONS: &[&str] =
    &["rs", "py", "pyi", "ts", "tsx", "js", "jsx", "c", "h", "cc", "cpp", "hpp", "go"];

const SKIP_DIRS: &[&str] =
    &[".git", ".tursi", ".venv", "target", "node_modules", "__pycache__", "dist", "build"];

#[derive(Debug, Clone, Copy, Default)]
pub struct PlanState {
    /// Planning phase: per-edit approval suspended (§5.5).
    pub active: bool,
    pub approved: bool,
    /// Stub count at `:approve` — the denominator of the fill-in meter.
    pub total_stubs: usize,
}

impl PlanState {
    /// `:plan` — the next task is a skeleton, not an implementation.
    pub fn enter(&mut self) {
        self.active = true;
        self.approved = false;
        self.total_stubs = 0;
    }

    /// `:approve` — plan phase ends, fill-in begins with this denominator.
    /// In AFK the gate's green auto-grants this (§5.5).
    pub fn approve(&mut self, total_stubs: usize) {
        self.active = false;
        self.approved = true;
        self.total_stubs = total_stubs;
    }
}

/// (filled, total) for the status bar. Before approval the meter shows raw
/// remaining stubs as the total.
pub fn progress(project: &Path, state: &PlanState) -> Result<(usize, usize)> {
    let remaining = count_stubs(project)?;
    Ok(if state.total_stubs > 0 {
        (state.total_stubs.saturating_sub(remaining), state.total_stubs)
    } else {
        (0, remaining)
    })
}

/// Count stub markers across the project's source files, skipping string
/// literals and comment lines.
pub fn count_stubs(project: &Path) -> Result<usize> {
    let mut files = Vec::new();
    walk(project, &mut files)?;
    let mut count = 0;
    for file in files {
        let Ok(content) = std::fs::read_to_string(&file) else { continue };
        for line in content.lines() {
            let trimmed = line.trim_start();
            if trimmed.starts_with("//") || trimmed.starts_with('#') || trimmed.starts_with('*') {
                continue;
            }
            for marker in STUB_MARKERS {
                count += occurrences_outside_strings(line, marker);
            }
        }
    }
    Ok(count)
}

/// Odd number of quotes before the match = the match sits inside a string.
/// Heuristic, not a lexer — good enough for a status meter.
fn occurrences_outside_strings(line: &str, marker: &str) -> usize {
    line.match_indices(marker)
        .filter(|(idx, _)| line[..*idx].matches('"').count() % 2 == 0)
        .count()
}

fn walk(dir: &Path, out: &mut Vec<PathBuf>) -> Result<()> {
    for entry in std::fs::read_dir(dir)?.flatten() {
        let path = entry.path();
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if path.is_dir() {
            if !SKIP_DIRS.contains(&name.as_ref()) && !name.starts_with('.') {
                walk(&path, out)?;
            }
        } else if path
            .extension()
            .and_then(|e| e.to_str())
            .is_some_and(|e| SOURCE_EXTENSIONS.contains(&e))
        {
            out.push(path);
        }
    }
    Ok(())
}

/// The plan gate: typecheck only (§5.5). Ok(Err(extract)) carries the red
/// output for the model; a plan cannot pass vacuously.
pub async fn typecheck_gate(
    sandbox: &Sandbox,
    typecheck: &[String],
) -> Result<std::result::Result<(), String>> {
    if typecheck.is_empty() {
        return Ok(Err(
            "no typecheck commands configured — set `typecheck` in .tursi/config.toml".to_string(),
        ));
    }
    let steps = typecheck
        .iter()
        .map(|c| Step {
            command: c.clone(),
            cwd: None,
            env: vec![],
            streams: Streams::Auto,
            timeout: sandbox.timeout_cap,
            tail_lines: 30,
        })
        .collect();
    let results = sandbox.run_steps(steps, OnError::Stop).await?;
    match results.iter().find(|r| r.ran && r.exit_code != Some(0)) {
        None => Ok(Ok(())),
        Some(red) => Ok(Err(format!(
            "{}: exit {:?} — {}",
            red.command,
            red.exit_code,
            output::truncate(&format!("{}\n{}", red.stderr, red.stdout), 30)
        ))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tools::testutil;

    #[test]
    fn stub_count_ignores_strings_and_comments() {
        let dir = testutil::tmp("plan-count");
        std::fs::write(
            dir.join("a.rs"),
            "fn real() {\n    todo!()\n}\n// todo!() in a comment\nconst S: &str = \"todo!()\";\n",
        )
        .unwrap();
        std::fs::write(dir.join("b.py"), "def f():\n    raise NotImplementedError\n").unwrap();
        std::fs::write(dir.join("notes.txt"), "todo!() not source\n").unwrap();
        assert_eq!(count_stubs(&dir).unwrap(), 2);
    }

    #[test]
    fn meter_uses_the_approval_denominator() {
        let dir = testutil::tmp("plan-meter");
        std::fs::write(dir.join("a.rs"), "fn x() { todo!() }\nfn y() { todo!() }\n").unwrap();
        let mut state = PlanState::default();
        state.enter();
        assert!(state.active);
        assert_eq!(progress(&dir, &state).unwrap(), (0, 2));
        state.approve(2);
        assert!(!state.active && state.approved);
        std::fs::write(dir.join("a.rs"), "fn x() { todo!() }\nfn y() { 42; }\n").unwrap();
        assert_eq!(progress(&dir, &state).unwrap(), (1, 2));
    }

    #[tokio::test]
    async fn gate_fails_without_typecheck_commands_and_reports_red() {
        let dir = testutil::tmp("plan-gate");
        let sandbox = crate::sandbox::Sandbox::for_tests(&dir);
        let empty = typecheck_gate(&sandbox, &[]).await.unwrap();
        assert!(empty.is_err());
        let green = typecheck_gate(&sandbox, &["true".to_string()]).await.unwrap();
        assert!(green.is_ok());
        let red = typecheck_gate(&sandbox, &["false".to_string()]).await.unwrap();
        assert!(red.unwrap_err().contains("false: exit"));
    }
}
