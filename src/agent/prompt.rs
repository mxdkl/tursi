//! Prompt assembly (§8.4): a byte-stable prefix, mode boundaries as user-role
//! injections, and compaction.

use anyhow::Result;
use std::collections::HashMap;
use std::path::PathBuf;

use crate::api::{Message, Provider, ToolCall};
use crate::syscard::SystemCard;

/// Default for `Config::context_window` — a conservative floor.
pub const DEFAULT_CONTEXT_WINDOW: u64 = 131_072;

/// Compaction leaves the most recent messages whole.
const KEEP_RECENT: usize = 10;
/// Payloads at or under this size aren't worth collapsing.
const COMPACT_MIN_BYTES: usize = 200;

/// The stable-for-the-session block after the core prompt and system card.
pub struct ProjectBlock {
    pub root: String,
    pub languages: Vec<String>,
    pub verify: Vec<String>,
    /// Headless (`--task`) runs edit the checkout in place — a script or grader
    /// reads it — instead of the worktree rule (PERMISSIONS.md §6).
    pub headless: bool,
}

/// Canonical text: PROMPT.md documents it, prompts/core.txt is the embedded
/// copy — edit both together.
pub fn core() -> &'static str {
    include_str!("../../prompts/core.txt")
}

/// core → system card → project block, byte-stable within a session (§8.4).
/// Anything that can change mid-session never enters this string.
pub fn prefix(card: &SystemCard, project: &ProjectBlock) -> String {
    format!(
        "{}\n## Machine\n{}\n## Project\nRoot: {}\nLanguages: {}\nVerify: {}\nWorkflow: {}\n",
        core(),
        card.render(),
        project.root,
        if project.languages.is_empty() { "unknown".to_string() } else { project.languages.join(", ") },
        if project.verify.is_empty() { "none configured".to_string() } else { project.verify.join(" && ") },
        if project.headless { "in place" } else { "worktree (rule 8)" },
    )
}

/// `[plan]` injection at plan-mode start (§5.5; text mirrored in PROMPT.md).
pub fn plan_injection() -> &'static str {
    "[plan] This task is a skeleton plan, not an implementation. Create every file; \
     write real signatures (names, typed parameters, return types); bodies contain only \
     the language's stub marker (todo!(), raise NotImplementedError, …). Orchestration \
     bodies are the exception: write their real call sequences. Data types get real \
     fields. Optionally add stubbed test functions named for acceptance criteria. The \
     skeleton must pass the typecheck. Implement nothing else."
}

/// `[plan]` injection at `:approve` (fill-in start).
pub fn fill_in_injection() -> &'static str {
    "[plan] Skeleton approved. Implement the remaining stubs; keep the approved \
     signatures unless impossible — if one must change, say so and why. Typecheck after \
     each file."
}

/// `[verify] {cmd}: exit {code} — {extract}` (§5.3).
pub fn verify_injection(command: &str, exit_code: i32, extract: &str) -> String {
    format!("[verify] {command}: exit {exit_code} — {extract}")
}

/// Injected when an AFK final turn changed no files and said nothing — that's a
/// bail, not a completion (§5.3). Bounded by the caller so it can't loop.
pub fn empty_done_nudge() -> &'static str {
    "[continue] You ended your turn without changing any files and without a summary — that \
     is not a completed task. Implement the change now; if you are genuinely blocked, say \
     exactly what is blocking you and the next thing you would try."
}

/// Injected — mid-task (proactively) or at a done-claim — when files were
/// changed but nothing has run against them since (§5.3). Worded to fit both.
/// Bounded by the caller so it can't loop.
pub fn run_before_done_nudge() -> &'static str {
    "[continue] You've changed files but haven't run anything against them. Actually exercise \
     your change now — build it, run the tests, or render/execute the new behavior on a \
     concrete example. Reading the code back is not verifying it."
}

/// Compaction trigger: crude byte/4 estimate vs. the context window (§8).
pub fn should_compact(transcript_tokens: u64, context_window: u64, fraction: f32) -> bool {
    transcript_tokens as f64 >= context_window as f64 * fraction as f64
}

/// Share of the context that is collapsible: old tool outputs and write/edit
/// payloads `compact` would shrink. Compaction busts the provider's cache once;
/// worth it only when it removes a large share.
const COLLAPSIBLE_SHARE: f64 = 0.4;

/// The early trigger (§8): past `min_tokens` and mostly bloat.
pub fn should_compact_early(transcript: &[Message], context_tokens: u64, min_tokens: u64) -> bool {
    if min_tokens == 0 || context_tokens < min_tokens {
        return false;
    }
    let keep_from = transcript.len().saturating_sub(KEEP_RECENT);
    let collapsible: usize = transcript[..keep_from]
        .iter()
        .map(|m| match m {
            Message::ToolResult { content, .. } if content.len() > COMPACT_MIN_BYTES => {
                content.len() - content.lines().next().map_or(0, str::len)
            }
            Message::Assistant { tool_calls, .. } => tool_calls
                .iter()
                .filter(|c| matches!(c.name.as_str(), "write" | "edit"))
                .map(|c| c.arguments.to_string().len().saturating_sub(COMPACT_MIN_BYTES))
                .sum(),
            _ => 0,
        })
        .sum();
    (collapsible / 4) as f64 >= context_tokens as f64 * COLLAPSIBLE_SHARE
}

/// v0, no model-written summary yet: collapse the bulk older than the last
/// KEEP_RECENT messages — tool outputs (to their first line plus any log
/// pointers) and the payloads of write/edit calls — the one cache-busting hit
/// (§8). Returns the files whose content left the context, so the caller can
/// stop `read` answering "unchanged" for them.
pub async fn compact(_provider: &Provider, transcript: &mut [Message]) -> Result<Vec<PathBuf>> {
    let keep_from = transcript.len().saturating_sub(KEEP_RECENT);
    // call id → (tool, files it touched), so a result knows what produced it.
    let calls: HashMap<String, (String, Vec<PathBuf>)> = transcript
        .iter()
        .filter_map(|m| match m {
            Message::Assistant { tool_calls, .. } => Some(tool_calls),
            _ => None,
        })
        .flatten()
        .map(|c| (c.id.clone(), (c.name.clone(), touched_files(c))))
        .collect();

    let mut forgotten = Vec::new();
    for message in &mut transcript[..keep_from] {
        match message {
            Message::ToolResult { call_id, content, .. } if content.len() > COMPACT_MIN_BYTES => {
                let mut short = content.lines().next().unwrap_or("").to_string();
                match calls.get(call_id.as_str()) {
                    // read output never goes to debug.log — the file itself is
                    // the recovery path.
                    Some((tool, files)) if tool == "read" => {
                        short.push_str("\n[compacted — read the file again if you need it]");
                        forgotten.extend(files.iter().cloned());
                    }
                    _ => {
                        for line in content.lines().skip(1).filter(|l| l.contains("[full: log#")) {
                            short.push('\n');
                            short.push_str(line);
                        }
                        short.push_str("\n[compacted — log_search recovers the full output]");
                    }
                }
                *content = short;
            }
            Message::Assistant { tool_calls, .. } => {
                for call in tool_calls.iter_mut() {
                    if compact_payload(call) {
                        forgotten.extend(touched_files(call));
                    }
                }
            }
            _ => {}
        }
    }
    Ok(forgotten)
}

/// Files a read/write/edit call addressed, as the model spelled them.
fn touched_files(call: &ToolCall) -> Vec<PathBuf> {
    let a = &call.arguments;
    let list = |key: &str| {
        a.get(key)
            .and_then(|v| v.as_array())
            .map(|items| items.iter().filter_map(|i| i.get("file")?.as_str().map(PathBuf::from)).collect())
            .unwrap_or_default()
    };
    match call.name.as_str() {
        "read" => list("reads"),
        "edit" => list("edits"),
        "write" => a.get("file").and_then(|f| f.as_str()).map(PathBuf::from).into_iter().collect(),
        _ => Vec::new(),
    }
}

/// Collapse a write's content / an edit's strings in place, keeping the
/// arguments valid JSON for the wire. True if anything changed.
fn compact_payload(call: &mut ToolCall) -> bool {
    let collapse = |v: &mut serde_json::Value, what: &str| {
        let Some(text) = v.as_str().filter(|t| t.len() > COMPACT_MIN_BYTES) else { return false };
        *v = serde_json::Value::String(format!("[compacted: {} lines {what}]", text.lines().count()));
        true
    };
    let a = &mut call.arguments;
    match call.name.as_str() {
        "write" => a.get_mut("content").is_some_and(|c| collapse(c, "written")),
        "edit" => {
            let mut changed = false;
            for hunk in a.get_mut("edits").and_then(|e| e.as_array_mut()).into_iter().flatten() {
                for key in ["old_string", "new_string"] {
                    if let Some(v) = hunk.get_mut(key) {
                        changed |= collapse(v, "replaced");
                    }
                }
            }
            changed
        }
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// PROMPT.md documents the prompt; the code is the source. Both halves —
    /// the core prompt quote and the injection texts — must match it.
    #[test]
    fn prompt_md_quotes_core_txt_verbatim() {
        let doc = include_str!("../../PROMPT.md");
        let quoted: Vec<&str> = doc
            .lines()
            .skip_while(|l| *l != "<!-- core.txt:begin -->")
            .skip(1)
            .take_while(|l| *l != "<!-- core.txt:end -->")
            .map(|l| l.strip_prefix("> ").or_else(|| l.strip_prefix('>')).unwrap_or(l))
            .collect();
        assert_eq!(quoted.join("\n"), core().trim_end(), "PROMPT.md's core prompt drifted from prompts/core.txt");

        let flat = |s: &str| s.split_whitespace().filter(|w| *w != ">").collect::<Vec<_>>().join(" ");
        let doc = flat(doc);
        for injection in [plan_injection(), fill_in_injection(), empty_done_nudge(), run_before_done_nudge()] {
            assert!(doc.contains(&flat(injection)), "PROMPT.md is missing or misquotes: {injection}");
        }
    }

    #[test]
    fn early_compaction_waits_for_size_and_bloat() {
        let big = Message::ToolResult { call_id: "x".into(), content: "first\n".to_string() + &"y".repeat(4000), is_error: false };
        let mut transcript: Vec<Message> = (0..10).map(|_| big.clone()).collect();
        transcript.extend((0..KEEP_RECENT).map(|i| Message::User(format!("recent {i}"))));
        // 10 × 4k bytes of old output ≈ 10k tokens of a 12k context: bloated —
        // but below the size floor, nothing happens.
        assert!(!should_compact_early(&transcript, 12_000, 24_000));
        assert!(should_compact_early(&transcript, 12_000, 10_000));
        // Mostly prose, not collapsible: stays.
        let prose: Vec<Message> = (0..40).map(|i| Message::User(format!("note {i} ") + &"z".repeat(3000))).collect();
        assert!(!should_compact_early(&prose, 30_000, 10_000));
        assert!(!should_compact_early(&transcript, 12_000, 0), "0 disables");
    }

    fn call(id: &str, name: &str, arguments: serde_json::Value) -> ToolCall {
        ToolCall { id: id.into(), name: name.into(), arguments, malformed: None }
    }

    #[tokio::test]
    async fn compaction_collapses_old_payloads_and_reports_forgotten_files() {
        let big = "x\n".repeat(300);
        let mut transcript = vec![
            Message::Assistant {
                text: String::new(),
                tool_calls: vec![
                    call("r", "read", json!({"reads": [{"file": "a.rs"}]})),
                    call("w", "write", json!({"file": "b.rs", "content": big})),
                    call("e", "execute_command", json!({"steps": [{"command": "cargo test"}]})),
                ],
            },
            Message::ToolResult { call_id: "r".into(), content: format!("── a.rs ──\n{big}"), is_error: false },
            Message::ToolResult { call_id: "w".into(), content: "wrote b.rs (300 lines)".into(), is_error: false },
            Message::ToolResult {
                call_id: "e".into(),
                content: format!("1 ✓ cargo build (1.0s) [full: log#1]\n{big}2 ✗ cargo test exit 101 (0.5s) [full: log#9]"),
                is_error: false,
            },
        ];
        transcript.extend((0..KEEP_RECENT).map(|i| Message::User(format!("recent {i}"))));
        let provider = Provider::OpenAiCompat(crate::api::openai_compat::Client::new(String::new(), String::new()));

        let mut forgotten = compact(&provider, &mut transcript).await.unwrap();
        forgotten.sort();
        assert_eq!(forgotten, vec![PathBuf::from("a.rs"), PathBuf::from("b.rs")]);

        let Message::Assistant { tool_calls, .. } = &transcript[0] else { panic!() };
        assert_eq!(tool_calls[1].arguments["content"], "[compacted: 300 lines written]");
        let Message::ToolResult { content, .. } = &transcript[1] else { panic!() };
        assert!(content.contains("read the file again") && !content.contains("log_search"), "{content}");
        let Message::ToolResult { content, .. } = &transcript[3] else { panic!() };
        assert!(content.contains("log#1") && content.contains("log#9"), "log pointers kept: {content}");
        assert!(content.len() < 300, "collapsed: {content}");

        // Idempotent: a second pass finds nothing new to forget.
        assert!(compact(&provider, &mut transcript).await.unwrap().is_empty());
    }
}
