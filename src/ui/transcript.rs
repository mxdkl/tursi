//! The chat as structured entries: `❯` for the user, `●` for the lead's
//! prose, dim one-line notes, and a `✻` footer when a task ends. Tool calls
//! never appear here — the lead's are hidden, and subagents' work shows in
//! their tiles (`ui::agents`).

use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use std::time::Duration;

pub enum Entry {
    User(String),
    /// A block of the lead's text (one turn's prose, or a streamed partial).
    Agent(String),
    /// Task end: `✻ Worked for 1m 12s · done 08:20 · bal $7.80 · ctx 23k`.
    Done { ok: bool, elapsed: Duration, at: String, cost_usd: f64, balance_usd: Option<f64>, ctx_tokens: u64 },
    /// Harness notes (goal verdicts, `/log` output).
    Note(String),
    /// Something woke the lead (a subagent report, a monitor): its first line.
    Wake(String),
}

const VERBS: &[&str] = &["Worked", "Cooked", "Simmered", "Tinkered", "Hammered", "Sautéed", "Brewed"];

const DIM: Style = Style::new().fg(Color::DarkGray);

/// Render entries to styled lines for a `width`-column view.
pub fn render(entries: &[Entry], width: usize) -> Vec<Line<'static>> {
    let width = width.max(20);
    let mut out = Vec::new();
    for entry in entries {
        match entry {
            Entry::User(text) => {
                out.push(Line::raw(""));
                block(&mut out, "❯ ", text, Style::new().add_modifier(Modifier::BOLD), Style::new().add_modifier(Modifier::BOLD), width);
            }
            Entry::Agent(text) => {
                out.push(Line::raw(""));
                block(&mut out, "● ", text, Style::new(), Style::new(), width);
            }
            Entry::Done { ok, elapsed, at, cost_usd, balance_usd, ctx_tokens } => {
                out.push(Line::raw(""));
                let verb = VERBS[(elapsed.as_secs() as usize) % VERBS.len()];
                let status = if *ok { "done" } else { "stopped" };
                let money = match balance_usd {
                    Some(bal) => format!("bal ${bal:.2}"),
                    None => format!("${cost_usd:.3}"),
                };
                out.push(Line::styled(
                    clip(&format!("✻ {verb} for {} · {status} {at} · {money} · ctx {}k", human(*elapsed), ctx_tokens / 1000), width),
                    Style::new().fg(if *ok { Color::DarkGray } else { Color::Red }).add_modifier(Modifier::ITALIC),
                ));
            }
            Entry::Note(text) => {
                for line in text.lines() {
                    out.push(Line::styled(clip(line, width), DIM));
                }
            }
            Entry::Wake(text) => {
                let first = text.lines().next().unwrap_or("").trim_start_matches('[').trim_end_matches(']');
                out.push(Line::styled(clip(&format!("⚡ {}", wake_line(first)), width), Style::new().fg(Color::Yellow)));
            }
        }
    }
    out
}

/// A subagent report's head reads `agent-3 writer <task> finished`: the lead
/// needs the id to follow up; the chat shows `writer finished: <task>`.
fn wake_line(first: &str) -> String {
    let Some(rest) = first.strip_prefix("agent-") else { return first.to_string() };
    let Some((_, rest)) = rest.split_once(' ') else { return first.to_string() };
    let (role, task) = rest.split_once(' ').unwrap_or((rest, ""));
    let task = task.strip_suffix(" finished").unwrap_or(task).trim();
    if task.is_empty() { format!("{role} finished") } else { format!("{role} finished: {task}") }
}

/// Rebuild the chat from a persisted transcript (`--resume`): your prompts
/// and the lead's prose. Tool calls and results stay hidden; harness
/// injections become one-line notes.
pub fn from_messages(messages: &[crate::api::Message]) -> Vec<Entry> {
    use crate::api::Message;
    let mut out = Vec::new();
    for m in messages {
        match m {
            Message::User(text) if text.starts_with('[') => out.push(Entry::Wake(text.clone())),
            Message::User(text) => out.push(Entry::User(text.clone())),
            Message::Assistant { text, .. } if !text.trim().is_empty() => out.push(Entry::Agent(text.trim_end().to_string())),
            _ => {}
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
    fn the_chat_shows_prose_notes_and_the_footer() {
        let entries = vec![
            Entry::User("fix the bug".into()),
            Entry::Agent("Handing the fix to a writer.".into()),
            Entry::Wake("[agent-1 writer fix the bug finished]\nlong report body".into()),
            Entry::Done { ok: true, elapsed: Duration::from_secs(462), at: "8:20 AM".into(), cost_usd: 0.0123, balance_usd: Some(7.8), ctx_tokens: 23_400 },
        ];
        let t = text(&render(&entries, 80));
        assert!(t.contains(&"❯ fix the bug".to_string()));
        assert!(t.contains(&"● Handing the fix to a writer.".to_string()));
        assert!(t.contains(&"⚡ writer finished: fix the bug".to_string()), "{t:?}");
        assert!(!t.iter().any(|l| l.contains("long report body")), "a report goes to the lead, not the chat");
        assert!(t.iter().any(|l| l.contains("for 7m 42s · done 8:20 AM · bal $7.80 · ctx 23k")), "{t:?}");
    }

    #[test]
    fn replay_from_messages_hides_tool_traffic() {
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
        assert_eq!(entries.len(), 3);
        assert!(matches!(&entries[0], Entry::User(t) if t == "add a test"));
        assert!(matches!(&entries[1], Entry::Agent(t) if t == "Looking."));
        assert!(matches!(&entries[2], Entry::Wake(t) if t.starts_with("[verify]")));
    }

    #[test]
    fn agent_text_wraps_with_an_aligned_indent() {
        let entries = vec![Entry::Agent("one two three four five six seven".into())];
        let t = text(&render(&entries, 20)); // 18 columns after the bullet
        assert_eq!(t[1], "● one two three four");
        assert_eq!(t[2], "  five six seven");
    }
}
