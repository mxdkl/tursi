//! The transcript as structured entries, rendered Claude-Code style: `❯` for
//! the user, `●` blocks for the agent and its tool calls, `⎿` results that
//! collapse to a few lines (Ctrl+O expands), line-numbered diffs under
//! `Updated <file> (+a -b)`, and a `✻` footer when a task ends.

use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use std::time::Duration;

use crate::diff::{FileDiff, Kind};

pub enum Entry {
    User(String),
    /// A block of model text (one turn's prose, or a streamed partial).
    Agent(String),
    Tool { name: String, summary: String, result: Option<ToolResult> },
    Diff(FileDiff),
    /// Task end: `✻ Worked for 1m 12s · done 08:20 · $0.012 · ctx 23k`.
    Done { ok: bool, elapsed: Duration, at: String, cost_usd: f64, ctx_tokens: u64 },
    /// Harness notes (`:log` output).
    Note(String),
    /// A monitor fired while idle and started this turn.
    Wake(String),
}

pub struct ToolResult {
    pub content: String,
    pub is_error: bool,
}

/// Result lines shown when collapsed.
const COLLAPSED_LINES: usize = 3;
const VERBS: &[&str] = &["Worked", "Cooked", "Simmered", "Tinkered", "Hammered", "Sautéed", "Brewed"];

const DIM: Style = Style::new().fg(Color::DarkGray);

/// Render entries to styled lines for a `width`-column view. `verbose` shows
/// every result line instead of the first few.
pub fn render(entries: &[Entry], width: usize, verbose: bool) -> Vec<Line<'static>> {
    let width = width.max(20);
    let mut out = Vec::new();
    for (idx, entry) in entries.iter().enumerate() {
        match entry {
            Entry::User(text) => {
                out.push(Line::raw(""));
                block(&mut out, "❯ ", text, Style::new().add_modifier(Modifier::BOLD), Style::new().add_modifier(Modifier::BOLD), width);
            }
            Entry::Agent(text) => {
                out.push(Line::raw(""));
                block(&mut out, "● ", text, Style::new(), Style::new(), width);
            }
            Entry::Tool { name, summary, result } => {
                out.push(Line::raw(""));
                let bullet_style = match result {
                    None => Style::new().fg(Color::Yellow),
                    Some(r) if r.is_error => Style::new().fg(Color::Red),
                    Some(_) => Style::new().fg(Color::Green),
                };
                let head = if summary.is_empty() { name.clone() } else { format!("{name}({})", clip(summary, width.saturating_sub(name.len() + 5))) };
                out.push(Line::from(vec![Span::styled("● ", bullet_style), Span::styled(head, Style::new().add_modifier(Modifier::BOLD))]));
                // A successful edit/write is told by its diff; the "applied N
                // hunk(s)" line only repeats it (verbose shows both).
                let diff_follows = matches!(entries.get(idx + 1), Some(Entry::Diff(_)));
                if let Some(r) = result.as_ref().filter(|r| r.is_error || verbose || !diff_follows) {
                    let style = if r.is_error { Style::new().fg(Color::Red) } else { DIM };
                    let lines: Vec<&str> = r.content.lines().collect();
                    let shown = if verbose { lines.len() } else { lines.len().min(COLLAPSED_LINES) };
                    if lines.is_empty() {
                        out.push(Line::from(vec![Span::styled("  ⎿  ", DIM), Span::styled("(no output)", DIM)]));
                    }
                    for (i, line) in lines.iter().take(shown).enumerate() {
                        let prefix = if i == 0 { "  ⎿  " } else { "     " };
                        out.push(Line::from(vec![Span::styled(prefix, DIM), Span::styled(clip(line, width.saturating_sub(5)), style)]));
                    }
                    if lines.len() > shown {
                        out.push(Line::styled(format!("     … +{} lines (ctrl+o to expand)", lines.len() - shown), DIM));
                    }
                }
            }
            Entry::Diff(d) => {
                out.push(Line::from(vec![
                    Span::styled("  ⎿  ", DIM),
                    Span::styled(format!("Updated {} ", d.file), Style::new().add_modifier(Modifier::BOLD)),
                    Span::styled(format!("(+{} ", d.added), Style::new().fg(Color::Green)),
                    Span::styled(format!("-{})", d.removed), Style::new().fg(Color::Red)),
                ]));
                let num_w = d.lines.iter().filter_map(|l| l.new_no.or(l.old_no)).max().unwrap_or(0).to_string().len().max(2);
                for line in &d.lines {
                    out.push(match line.kind {
                        Kind::Gap => Line::styled(format!("     {:>num_w$}", "..."), DIM),
                        Kind::Context => Line::from(vec![
                            Span::styled(format!("     {:>num_w$}  ", line.new_no.unwrap_or(0)), DIM),
                            Span::raw(clip(&line.text, width.saturating_sub(num_w + 8))),
                        ]),
                        Kind::Removed => Line::from(vec![
                            Span::styled(format!("     {:>num_w$} ", line.old_no.unwrap_or(0)), DIM),
                            Span::styled(format!("-{}", clip(&line.text, width.saturating_sub(num_w + 8))), Style::new().fg(Color::Red)),
                        ]),
                        Kind::Added => Line::from(vec![
                            Span::styled(format!("     {:>num_w$} ", line.new_no.unwrap_or(0)), DIM),
                            Span::styled(format!("+{}", clip(&line.text, width.saturating_sub(num_w + 8))), Style::new().fg(Color::Green)),
                        ]),
                    });
                }
            }
            Entry::Done { ok, elapsed, at, cost_usd, ctx_tokens } => {
                out.push(Line::raw(""));
                let verb = VERBS[(elapsed.as_secs() as usize) % VERBS.len()];
                let status = if *ok { "done" } else { "stopped" };
                out.push(Line::styled(
                    format!("✻ {verb} for {} · {status} {at} · ${cost_usd:.3} · ctx {}k", human(*elapsed), ctx_tokens / 1000),
                    Style::new().fg(if *ok { Color::DarkGray } else { Color::Red }).add_modifier(Modifier::ITALIC),
                ));
            }
            Entry::Note(text) => {
                for line in text.lines() {
                    out.push(Line::styled(line.to_string(), DIM));
                }
            }
            Entry::Wake(text) => {
                out.push(Line::raw(""));
                block(&mut out, "⚡ ", text, Style::new().fg(Color::Yellow), Style::new().fg(Color::Yellow), width);
            }
        }
    }
    out
}

/// Rebuild entries from a persisted transcript (`--resume`): the model's own
/// view of the session — user prompts, prose, tool calls with their results.
/// Harness injections keep their bracketed form as notes.
pub fn from_messages(messages: &[crate::api::Message]) -> Vec<Entry> {
    use crate::api::Message;
    let mut out = Vec::new();
    for m in messages {
        match m {
            Message::System(_) => {}
            Message::User(text) => {
                if text.starts_with('[') {
                    out.push(Entry::Note(text.clone()));
                } else {
                    out.push(Entry::User(text.clone()));
                }
            }
            Message::Assistant { text, tool_calls } => {
                if !text.trim().is_empty() {
                    out.push(Entry::Agent(text.trim_end().to_string()));
                }
                for call in tool_calls {
                    let a = &call.arguments;
                    let summary = [a.pointer("/steps/0/command"), a.pointer("/reads/0/file"), a.pointer("/edits/0/file"), a.get("file"), a.get("pattern"), a.get("label")]
                        .into_iter()
                        .flatten()
                        .find_map(|v| v.as_str())
                        .unwrap_or("")
                        .chars()
                        .take(60)
                        .collect();
                    out.push(Entry::Tool { name: call.name.clone(), summary, result: None });
                }
            }
            Message::ToolResult { content, is_error, .. } => {
                if let Some(Entry::Tool { result, .. }) = out.iter_mut().rev().find(|e| matches!(e, Entry::Tool { result: None, .. })) {
                    *result = Some(ToolResult { content: content.clone(), is_error: *is_error });
                }
            }
        }
    }
    out
}

/// A bullet block: first line prefixed, the rest indented to align, wrapped.
fn block(out: &mut Vec<Line<'static>>, prefix: &str, text: &str, prefix_style: Style, style: Style, width: usize) {
    let indent = " ".repeat(prefix.chars().count());
    let inner = width.saturating_sub(indent.len()).max(10);
    let mut first = true;
    for para in text.lines() {
        let rows = if para.trim().is_empty() { vec![String::new()] } else { wrap(para, inner) };
        for row in rows {
            let lead = if first { Span::styled(prefix.to_string(), prefix_style) } else { Span::raw(indent.clone()) };
            out.push(Line::from(vec![lead, Span::styled(row, style)]));
            first = false;
        }
    }
    if first {
        out.push(Line::from(vec![Span::styled(prefix.to_string(), prefix_style)]));
    }
}

/// Word wrap to `width` columns, breaking long words.
fn wrap(text: &str, width: usize) -> Vec<String> {
    let mut rows = Vec::new();
    let mut row = String::new();
    for word in text.split(' ') {
        let fits = row.chars().count() + usize::from(!row.is_empty()) + word.chars().count() <= width;
        if !fits && !row.is_empty() {
            rows.push(std::mem::take(&mut row));
        }
        let mut word = word;
        while word.chars().count() > width {
            let cut = word.char_indices().nth(width).map(|(i, _)| i).unwrap_or(word.len());
            rows.push(word[..cut].to_string());
            word = &word[cut..];
        }
        if !row.is_empty() {
            row.push(' ');
        }
        row.push_str(word);
    }
    rows.push(row);
    rows
}

fn clip(text: &str, max: usize) -> String {
    let max = max.max(4);
    if text.chars().count() <= max {
        text.to_string()
    } else {
        let cut: String = text.chars().take(max - 1).collect();
        format!("{cut}…")
    }
}

fn human(d: Duration) -> String {
    let s = d.as_secs();
    if s < 60 {
        format!("{s}s")
    } else if s < 3600 {
        format!("{}m {}s", s / 60, s % 60)
    } else {
        format!("{}h {}m", s / 3600, (s % 3600) / 60)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn text(lines: &[Line]) -> Vec<String> {
        lines.iter().map(|l| l.spans.iter().map(|s| s.content.as_ref()).collect::<String>()).collect()
    }

    #[test]
    fn tool_results_collapse_and_expand() {
        let entries = vec![
            Entry::User("fix the bug".into()),
            Entry::Tool {
                name: "execute_command".into(),
                summary: "cargo test".into(),
                result: Some(ToolResult { content: (1..=8).map(|i| format!("line {i}")).collect::<Vec<_>>().join("\n"), is_error: false }),
            },
        ];
        let t = text(&render(&entries, 80, false));
        assert!(t.contains(&"❯ fix the bug".to_string()));
        assert!(t.contains(&"● execute_command(cargo test)".to_string()));
        assert!(t.contains(&"  ⎿  line 1".to_string()));
        assert!(t.contains(&"     line 3".to_string()));
        assert!(t.contains(&"     … +5 lines (ctrl+o to expand)".to_string()), "{t:?}");
        let v = text(&render(&entries, 80, true));
        assert!(v.contains(&"     line 8".to_string()) && !v.iter().any(|l| l.contains("expand")));
    }

    #[test]
    fn diffs_show_numbered_lines_and_the_footer_formats_time() {
        let d = crate::diff::diff("BENCH.md", "a\nb\nc\n", "a\nB\nc\n");
        let entries = vec![Entry::Diff(d), Entry::Done { ok: true, elapsed: Duration::from_secs(462), at: "8:20 AM".into(), cost_usd: 0.0123, ctx_tokens: 23_400 }];
        let t = text(&render(&entries, 80, false));
        assert!(t.iter().any(|l| l.contains("Updated BENCH.md (+1 -1)")), "{t:?}");
        assert!(t.contains(&"      2 -b".to_string()), "{t:?}");
        assert!(t.contains(&"      2 +B".to_string()), "{t:?}");
        assert!(t.iter().any(|l| l.contains("for 7m 42s · done 8:20 AM · $0.012 · ctx 23k")), "{t:?}");
    }

    /// Prints a sample session (`cargo test sample_session -- --nocapture`)
    /// and checks its skeleton: the quickest way to eyeball the look.
    #[test]
    fn sample_session_renders_in_the_expected_shape() {
        let d = crate::diff::diff("BENCH.md", "## Comparators\n\n- tursi (full config)\n- aider\n", "## Comparators\n\n- tursi (default config)\n- aider\n");
        let entries = vec![
            Entry::User("committed. reinstall and rebuild the bench binary".into()),
            Entry::Tool {
                name: "execute_command".into(),
                summary: "cargo install --path . --locked".into(),
                result: Some(ToolResult { content: "1 ✓ cargo install --path . --locked    (8.4s) [full: log#0]\n     Replacing /home/player1/.cargo/bin/tursi\n     Replaced package `tursi v0.1.0`\n  -rwxr-xr-x 1 player1 14176016 tursi".into(), is_error: false }),
            },
            Entry::Tool { name: "edit".into(), summary: "BENCH.md".into(), result: Some(ToolResult { content: "applied 1 hunk(s) to BENCH.md".into(), is_error: false }) },
            Entry::Diff(d),
            Entry::Tool { name: "execute_command".into(), summary: "cargo test".into(), result: Some(ToolResult { content: "1 ✗ cargo test    exit 101 (2.1s)\n  error[E0425]: cannot find value `ui`".into(), is_error: true }) },
            Entry::Agent("Both binaries are rebuilt from the committed tree and verified:\n\n- ~/.cargo/bin/tursi: reinstalled.\n- target-bookworm/release/tursi: rebuilt.".into()),
            Entry::Done { ok: true, elapsed: Duration::from_secs(55), at: "8:23 AM".into(), cost_usd: 0.0021, ctx_tokens: 5_800 },
        ];
        let t = text(&render(&entries, 100, false));
        eprintln!("\n{}\n", t.join("\n"));
        assert_eq!(t.iter().filter(|l| l.starts_with("● ")).count(), 4);
        assert!(t.iter().any(|l| l.starts_with("  ⎿  Updated BENCH.md (+1 -1)")));
        assert!(!t.iter().any(|l| l.contains("applied 1 hunk")), "the diff replaces the edit's result line");
        assert!(t.iter().any(|l| l.starts_with("✻ ")));
    }

    #[test]
    fn replay_from_messages_pairs_calls_with_results() {
        use crate::api::{Message, ToolCall};
        let messages = vec![
            Message::User("add a test".into()),
            Message::Assistant {
                text: "Looking.".into(),
                tool_calls: vec![ToolCall { id: "c1".into(), name: "read".into(), arguments: serde_json::json!({"reads": [{"file": "src/lib.rs"}]}), malformed: None }],
            },
            Message::ToolResult { call_id: "c1".into(), content: "── src/lib.rs ──\n1→fn x() {}".into(), is_error: false },
            Message::User("[verify] cargo test: exit 101 — …".into()),
        ];
        let entries = from_messages(&messages);
        assert!(matches!(&entries[0], Entry::User(t) if t == "add a test"));
        assert!(matches!(&entries[1], Entry::Agent(t) if t == "Looking."));
        assert!(matches!(&entries[2], Entry::Tool { name, summary, result: Some(r) } if name == "read" && summary == "src/lib.rs" && !r.is_error));
        assert!(matches!(&entries[3], Entry::Note(t) if t.starts_with("[verify]")));
    }

    #[test]
    fn agent_text_wraps_with_an_aligned_indent() {
        let entries = vec![Entry::Agent("one two three four five six seven".into())];
        let t = text(&render(&entries, 20, false)); // 18 columns after the bullet
        assert_eq!(t[1], "● one two three four");
        assert_eq!(t[2], "  five six seven");
    }
}
