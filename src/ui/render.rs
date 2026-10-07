//! Frame layout (§2): a header line, the chat column (transcript, input,
//! status line), and the agent block — still braille where each working
//! subagent lights one colored dot — beside the chat on wide terminals,
//! above it on narrow ones. It is always there, so the chat never resizes.
//! Overlays on top.

use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, Paragraph};

use super::{App, Overlay};

/// At or above this width the chat sits beside the tiles; below, under them.
const SIDE_BY_SIDE: u16 = 110;

pub fn draw(frame: &mut Frame, app: &mut App) {
    let full = frame.area();
    let t = app.clock.elapsed().as_secs_f32();
    if full.width >= SIDE_BY_SIDE {
        // Three quarters chat (with the header above it), one quarter agent
        // block running the full height.
        let chat_w = full.width * 3 / 4;
        let [left, _gap, field_area] =
            Layout::horizontal([Constraint::Length(chat_w), Constraint::Length(1), Constraint::Min(10)]).areas(full);
        let [header_area, chat_area] = Layout::vertical([Constraint::Length(1), Constraint::Min(4)]).areas(left);
        header(frame, header_area, app);
        chat(frame, chat_area, app);
        super::agents::draw(frame, field_area, &app.tiles, t);
    } else {
        let [header_area, body] = Layout::vertical([Constraint::Length(1), Constraint::Min(4)]).areas(full);
        header(frame, header_area, app);
        let [field_area, chat_area] = Layout::vertical([Constraint::Percentage(25), Constraint::Min(8)]).areas(body);
        super::agents::draw(frame, field_area, &app.tiles, t);
        chat(frame, chat_area, app);
    }
    if app.overlay.is_some() {
        overlay(frame, full, app);
    }
}

fn header(frame: &mut Frame, area: Rect, app: &App) {
    let project = app.session.project.file_name().and_then(|n| n.to_str()).unwrap_or("?");
    let mut spans = vec![Span::styled(format!(" {project} "), Style::default().add_modifier(Modifier::BOLD)), Span::raw("│ ")];
    if app.plan_active {
        spans.push(Span::styled("PLAN ", Style::default().fg(Color::Magenta)));
    }
    if app.afk {
        spans.push(Span::styled("AFK ", Style::default().fg(Color::Cyan)));
    }
    spans.push(Span::raw(match app.balance_usd {
        Some(bal) => format!("{} │ bal ${bal:.2} │ ", app.model),
        None => format!("{} │ ${:.2} session │ ", app.model, app.session_usd),
    }));
    // Context-window gauge, colored as it nears the compaction threshold (§8).
    let pct = if app.ctx_window > 0 { (app.ctx_used as f64 / app.ctx_window as f64 * 100.0).round() as u64 } else { 0 };
    let ctx_color = match pct {
        p if p >= 75 => Color::Red,
        p if p >= 60 => Color::Yellow,
        _ => Color::DarkGray,
    };
    spans.push(Span::styled(format!("ctx {pct}%"), Style::default().fg(ctx_color)));
    // Subagent jobs are listed with the monitors but have their own tiles.
    let monitors: Vec<String> = app.monitors.iter().filter(|(id, _)| !id.starts_with("agent-")).map(|(id, label)| format!("{id} {label}")).collect();
    if !monitors.is_empty() {
        spans.push(Span::styled(format!(" │ ⏱ {}", monitors.join(", ")), Style::default().fg(Color::Yellow)));
    }
    if let Some((_, since, turns, _)) = &app.goal {
        let s = since.elapsed().as_secs();
        let elapsed = if s < 60 { format!("{s}s") } else { format!("{}m", s / 60) };
        spans.push(Span::styled(format!(" │ ◎ goal {elapsed} · {turns} turns"), Style::default().fg(Color::Cyan)));
    }
    if let Some((filled, total)) = app.stubs {
        spans.push(Span::styled(format!(" │ stubs {filled}/{total}"), Style::default().fg(Color::Magenta)));
    }
    frame.render_widget(Paragraph::new(Line::from(spans)).style(Style::default().bg(Color::Black)), area);
}

/// The chat column: transcript, input box, one status line.
fn chat(frame: &mut Frame, area: Rect, app: &mut App) {
    let input_h = input_height(app, area.width);
    let [transcript_area, input_area, status_area] =
        Layout::vertical([Constraint::Min(3), Constraint::Length(input_h), Constraint::Length(1)]).areas(area);
    transcript(frame, transcript_area, app);
    input_bar(frame, input_area, app);
    status_line(frame, status_area, app);
}

/// Bottom-anchored scrollback: scroll == 0 means "latest" (newbbs model) —
/// new lines append at the anchor and the view follows unless scrolled away.
fn transcript(frame: &mut Frame, area: Rect, app: &mut App) {
    let height = area.height as usize;
    app.view_height = height;
    let width = area.width.saturating_sub(2) as usize;
    let mut lines = super::transcript::render(&app.transcript, width);
    if !app.partial.trim().is_empty() {
        // The block being streamed, rendered as if already closed.
        let live = [super::transcript::Entry::Agent(app.partial.trim_end().to_string())];
        lines.extend(super::transcript::render(&live, width));
    }
    app.line_count = lines.len();
    let max_scroll = lines.len().saturating_sub(height);
    app.scroll = app.scroll.min(max_scroll);
    let end = lines.len() - app.scroll;
    let start = end.saturating_sub(height);
    frame.render_widget(Paragraph::new(lines[start..end].to_vec()).block(Block::default().padding(ratatui::widgets::Padding::left(1))), area);
}

/// What the line under the input says: a status message if there is one,
/// else the working line while a task runs, else the key hints.
fn status_line(frame: &mut Frame, area: Rect, app: &App) {
    let (text, style) = if let Some(status) = &app.status {
        let style = if status.starts_with('✗') { Style::default().fg(Color::Red) } else { Style::default().fg(Color::DarkGray) };
        (status.clone(), style)
    } else if app.task_running {
        let s = app.task_started.map(|t| t.elapsed().as_secs()).unwrap_or(0);
        let elapsed = if s < 60 { format!("{s}s") } else { format!("{}m {:02}s", s / 60, s % 60) };
        let running = app.tiles.iter().filter(|t| t.running()).count();
        let agents = match running {
            0 => String::new(),
            1 => " · 1 agent working".to_string(),
            n => format!(" · {n} agents working"),
        };
        (format!("⋯ working {elapsed} · {}{agents} · Esc interrupts", super::agents::steps(app.lead_steps)), Style::default().fg(Color::Yellow))
    } else {
        ("Enter send · /help commands · PgUp/PgDn scroll · Ctrl+C quit".to_string(), Style::default().fg(Color::DarkGray))
    };
    let width = area.width.saturating_sub(1) as usize;
    let text: String = text.chars().take(width).collect();
    frame.render_widget(Paragraph::new(Line::styled(format!(" {text}"), style)), area);
}

/// Longest the input box may grow (content rows, excluding borders) before it
/// stops growing and scrolls internally — so a big paste can't eat the screen.
const MAX_INPUT_ROWS: usize = 8;

/// Input box: 95% width, centered, min 20 — reads as a field, not a banner.
fn input_box_width(full_width: u16) -> u16 {
    (full_width * 19 / 20).max(20).min(full_width)
}

/// Columns available for text inside the box borders.
fn input_inner_width(full_width: u16) -> usize {
    input_box_width(full_width).saturating_sub(2).max(1) as usize
}

/// The buffer as it's shown, with the cursor's char index into it: one
/// leading space so text sits a space in from the border.
fn input_display(app: &App) -> (String, usize) {
    (format!(" {}", app.input), 1 + app.cursor)
}

/// Character-wrap `content` to `inner` columns and locate the cursor. Character
/// wrap (not word wrap) keeps the cursor arithmetic exact for an input field.
/// Always ≥1 row, and a row always exists for a cursor sitting just past a
/// filled row (so typing at a wrap boundary has a home).
fn wrap_input(content: &str, cursor: usize, inner: usize) -> (Vec<String>, usize, usize) {
    let inner = inner.max(1);
    let chars: Vec<char> = content.chars().collect();
    let mut rows: Vec<String> = if chars.is_empty() {
        vec![String::new()]
    } else {
        chars.chunks(inner).map(|c| c.iter().collect()).collect()
    };
    let cur_row = cursor / inner;
    let cur_col = cursor % inner;
    while rows.len() <= cur_row {
        rows.push(String::new());
    }
    (rows, cur_row, cur_col)
}

/// Box height (incl. borders) needed for the wrapped input, capped.
fn input_height(app: &App, full_width: u16) -> u16 {
    let (content, cursor) = input_display(app);
    let (rows, _, _) = wrap_input(&content, cursor, input_inner_width(full_width));
    rows.len().clamp(1, MAX_INPUT_ROWS) as u16 + 2
}

fn input_bar(frame: &mut Frame, area: Rect, app: &App) {
    let (title, border) = if app.task_running {
        (" steer the running task ", Style::default().fg(Color::Yellow))
    } else {
        (" message ", Style::default().fg(Color::Green))
    };
    let width = input_box_width(area.width);
    let box_area = Rect { x: area.x + (area.width - width) / 2, width, ..area };
    let inner = input_inner_width(area.width);
    let (content, cursor) = input_display(app);
    let (rows, cur_row, cur_col) = wrap_input(&content, cursor, inner);

    // Scroll internally so the cursor row stays visible when the content is
    // taller than the (capped) box.
    let visible = (box_area.height as usize).saturating_sub(2).max(1);
    let first = cur_row.saturating_sub(visible - 1);
    let shown: Vec<Line> = rows[first..(first + visible).min(rows.len())].iter().map(|r| Line::raw(r.clone())).collect();
    frame.render_widget(Paragraph::new(shown).block(Block::default().borders(Borders::ALL).title(title).border_style(border)), box_area);
    if app.overlay.is_none() {
        let x = box_area.x + 1 + cur_col as u16;
        let y = box_area.y + 1 + (cur_row - first) as u16;
        frame.set_cursor_position((x, y));
    }
}

/// Overlays render centered over the transcript: approval diff (y/n/a),
/// rejection reason, ask_user, help, sysinfo.
fn overlay(frame: &mut Frame, area: Rect, app: &App) {
    let popup = centered(area, 80, 70);
    frame.render_widget(Clear, popup);
    let (title, body, footer): (String, Vec<Line>, &str) = match app.overlay.as_ref().expect("checked") {
        Overlay::Approval(request) => {
            let mut lines = Vec::new();
            if request.warn {
                lines.push(Line::styled(
                    "⚠ deny-listed — this prompts in every mode",
                    Style::default().fg(Color::Red).add_modifier(Modifier::BOLD),
                ));
            }
            for line in request.diff.as_deref().unwrap_or("").lines() {
                lines.push(diff_line(line));
            }
            (
                format!(" {} ", request.summary),
                lines,
                "[y] allow   [n] refuse + reason   [Esc] refuse",
            )
        }
        Overlay::RejectReason(request, reason) => (
            format!(" reject: {} ", request.summary),
            vec![Line::raw(format!("reason (steers the model, §3.4): {reason}▌"))],
            "[Enter] send   [Esc] reject without reason",
        ),
        Overlay::Ask(request, buffer) => {
            let mut lines = vec![Line::styled(
                request.question.clone(),
                Style::default().add_modifier(Modifier::BOLD),
            )];
            for (i, option) in request.options.iter().enumerate() {
                lines.push(Line::raw(format!("  [{}] {option}", i + 1)));
            }
            lines.push(Line::raw(format!("> {buffer}▌")));
            (" the agent asks ".to_string(), lines, "[1-9] pick   type + [Enter] answer   [Esc] dismiss")
        }
        Overlay::Help => (
            " keymap ".to_string(),
            HELP.lines().map(Line::raw).collect(),
            "any key closes",
        ),
        Overlay::SysInfo => (
            " system card ".to_string(),
            app.card.render().lines().map(|l| Line::raw(l.to_string())).collect(),
            "any key closes",
        ),
    };
    let block = Block::default()
        .borders(Borders::ALL)
        .title(title)
        .title_bottom(Line::raw(footer).right_aligned());
    frame.render_widget(Paragraph::new(body).block(block), popup);
}

fn diff_line(line: &str) -> Line<'_> {
    let style = match line.chars().next() {
        Some('+') => Style::default().fg(Color::Green),
        Some('-') => Style::default().fg(Color::Red),
        Some('!') => Style::default().fg(Color::Red).add_modifier(Modifier::BOLD),
        Some('?') => Style::default().fg(Color::Yellow),
        _ => Style::default().fg(Color::DarkGray),
    };
    Line::styled(line, style)
}

fn centered(area: Rect, percent_x: u16, percent_y: u16) -> Rect {
    let [_, mid, _] = Layout::vertical([
        Constraint::Percentage((100 - percent_y) / 2),
        Constraint::Percentage(percent_y),
        Constraint::Percentage((100 - percent_y) / 2),
    ])
    .areas(area);
    let [_, popup, _] = Layout::horizontal([
        Constraint::Percentage((100 - percent_x) / 2),
        Constraint::Percentage(percent_x),
        Constraint::Percentage((100 - percent_x) / 2),
    ])
    .areas(mid);
    popup
}

const HELP: &str = "\
TYPE      Enter send (steers the lead while it works) · Esc interrupt
          Up/Down earlier messages · Ctrl+U clear · Ctrl+W delete word
          Ctrl+A/E start/end · PgUp/PgDn scroll · Ctrl+Home/End top/bottom
COMMANDS  /goal [condition|clear] · /plan [task] · /approve
          /afk · /model [id] · /monitors · /monitor stop <id>
          /log <pattern> · /sysinfo · /help · /quit
PROMPTS   network: y/n · questions: 1-9 or type an answer
          Ctrl+C clears the line, or quits when it's empty

The chat shows you and the lead. In the braille block beside it, each
working subagent lights one dot: blue for a reader, green for a writer.";

#[cfg(test)]
mod tests {
    use super::*;

    /// Draws a session with the lead and three subagents into an in-memory
    /// terminal (`cargo test frame_ -- --nocapture` prints it).
    fn frame_text(app: &mut App, w: u16, h: u16) -> String {
        let mut terminal = ratatui::Terminal::new(ratatui::backend::TestBackend::new(w, h)).unwrap();
        terminal.draw(|f| draw(f, app)).unwrap();
        let buf = terminal.backend().buffer().clone();
        (0..h).map(|y| (0..w).map(|x| buf[(x, y)].symbol().to_string()).collect::<String>().trim_end().to_string()).collect::<Vec<_>>().join("\n")
    }

    fn busy_app() -> App {
        use crate::bus::{AgentId, EventKind, ROOT, UiEvent};
        let (mut app, ..) = crate::ui::testui::app("render-frame");
        app.transcript.push(crate::ui::transcript::Entry::User("read the todo file and continue".into()));
        app.apply_event(UiEvent { agent: ROOT, kind: EventKind::AgentText("Three independent items: a writer for each, and a reader for the loader.".into()) });
        app.apply_event(UiEvent { agent: ROOT, kind: EventKind::ToolStarted { name: "agent".into(), summary: "writer: ...".into() } });
        app.task_running = true;
        app.task_started = Some(std::time::Instant::now());
        for (n, access, title) in [(1, "writer", "Add a step budget to the VM loop"), (2, "writer", "Fix the gather mask for inactive lanes"), (3, "reader", "Map how the ELF loader places segments")] {
            app.apply_event(UiEvent { agent: AgentId(n), kind: EventKind::SubagentStarted { access: access.into(), title: title.into(), model: "cloudflare/@cf/deepseek-ai/deepseek-v4-flash-0731".into(), continued: false } });
        }
        app.apply_event(UiEvent { agent: AgentId(1), kind: EventKind::ToolStarted { name: "read".into(), summary: "src/cpu.rs".into() } });
        app.apply_event(UiEvent { agent: AgentId(1), kind: EventKind::ToolStarted { name: "execute_command".into(), summary: "cargo test --release".into() } });
        app.apply_event(UiEvent { agent: AgentId(1), kind: EventKind::ToolFinished { name: "execute_command".into(), content: "1 ✗ cargo test exit 101".into(), is_error: true } });
        app.apply_event(UiEvent { agent: AgentId(2), kind: EventKind::AgentText("The mask is built from the wrong lane bits.\n".into()) });
        app.apply_event(UiEvent { agent: AgentId(3), kind: EventKind::SubagentFinished { ok: true } });
        app
    }

    fn field_colors(text_app: &mut App, w: u16, h: u16) -> Vec<(u8, u8, u8)> {
        let mut terminal = ratatui::Terminal::new(ratatui::backend::TestBackend::new(w, h)).unwrap();
        terminal.draw(|f| draw(f, text_app)).unwrap();
        terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .filter(|c| c.symbol().chars().all(|ch| ('\u{2800}'..='\u{28ff}').contains(&ch)))
            .filter_map(|c| match c.fg {
                Color::Rgb(r, g, b) => Some((r, g, b)),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn frame_wide_puts_the_chat_beside_the_agent_field() {
        let mut app = busy_app();
        let text = frame_text(&mut app, 150, 30);
        eprintln!("\n{text}\n");
        assert!(text.contains("❯ read the todo file and continue"));
        assert!(text.contains("● Three independent items"));
        for hidden in ["agent(writer", "Add a step budget", "$ cargo test", "src/mem.rs", "agent-"] {
            assert!(!text.contains(hidden), "{hidden:?} should not be on screen");
        }
        assert!(text.contains("2 writers"), "legend counts working agents by kind");
        assert!(text.contains("2 agents working"), "status line counts running agents");
        // Two working writers share one green cell, top-left of the block;
        // the finished explorer has faded once its fade time is past.
        if let Some(explorer) = app.tiles.iter_mut().find(|t| t.id == 3) {
            explorer.ended = Some(std::time::Instant::now() - std::time::Duration::from_secs(5));
        }
        let dots = field_colors(&mut app, 150, 30);
        assert_eq!(dots.len(), 1, "{dots:?}");
        assert!(dots.iter().all(|(r, g, b)| g > r && g > b), "writers are green");
        let braille = text.chars().filter(|c| ('\u{2801}'..='\u{28ff}').contains(c)).count();
        assert!(braille <= 2, "unlit dots are invisible");
        // The chat takes three quarters of the width.
        let input_right = text.lines().find(|l| l.contains("┌ steer")).and_then(|l| l.chars().position(|c| c == '┐')).unwrap();
        assert!(text.contains("┌ agents "), "the block is framed and titled");
        assert!(text.lines().next().unwrap().contains("┌ agents "), "the block starts on the top line, beside the header");
        assert!(input_right > 100 && input_right < 115, "chat ≈ 3/4 of 150 columns, got {input_right}");
    }

    #[test]
    fn frame_narrow_stacks_the_field_over_the_chat() {
        let mut app = busy_app();
        let text = frame_text(&mut app, 90, 40);
        eprintln!("\n{text}\n");
        let field_row = text.lines().position(|l| l.contains("2 writers")).unwrap();
        let chat_row = text.lines().position(|l| l.contains("❯ read the todo")).unwrap();
        assert!(field_row < chat_row, "field above the chat on a narrow terminal");
    }

    #[test]
    fn frame_keeps_the_pane_when_no_agent_is_working() {
        let (mut app, ..) = crate::ui::testui::app("render-plain");
        app.transcript.push(crate::ui::transcript::Entry::User("hello".into()));
        let idle = frame_text(&mut app, 150, 16);
        assert!(idle.contains("❯ hello") && idle.contains("Enter send · /help"));
        assert!(field_colors(&mut app, 150, 16).is_empty(), "no agents, no colored dots");
        assert!(idle.contains("┌ agents "), "an idle block still shows its frame");
        // The chat column is as wide with agents as without: it never resizes.
        let mut busy = busy_app();
        let with = frame_text(&mut busy, 150, 16);
        let input_width = |t: &str| {
            t.lines().find(|l| l.contains("┌ message") || l.contains("┌ steer")).and_then(|l| l.chars().position(|c| c == '┐')).unwrap_or(0)
        };
        assert!(input_width(&idle) > 0);
        assert_eq!(input_width(&idle), input_width(&with));
    }

    #[test]
    fn short_input_is_one_row_with_the_cursor_on_it() {
        let (rows, r, c) = wrap_input(" hello", 6, 20);
        assert_eq!(rows, vec![" hello".to_string()]);
        assert_eq!((r, c), (0, 6));
    }

    #[test]
    fn long_input_wraps_instead_of_running_off_screen() {
        // 25 chars into a 10-wide field → 3 rows, none wider than the field.
        let content = "0123456789abcdefghijABCDE";
        let (rows, cur_row, cur_col) = wrap_input(content, content.chars().count(), 10);
        assert_eq!(rows.len(), 3);
        assert!(rows.iter().all(|r| r.chars().count() <= 10), "no row exceeds the width");
        assert_eq!(rows[0], "0123456789");
        // Cursor after the 25th char sits on row 2, col 5.
        assert_eq!((cur_row, cur_col), (2, 5));
    }

    #[test]
    fn cursor_just_past_a_filled_row_gets_a_home_row() {
        // Exactly 10 chars, cursor at end (10) in a 10-wide field: a 2nd, empty
        // row must exist so the cursor has somewhere to sit.
        let (rows, cur_row, cur_col) = wrap_input("0123456789", 10, 10);
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[1], "");
        assert_eq!((cur_row, cur_col), (1, 0));
    }

    #[test]
    fn empty_input_still_has_one_row() {
        let (rows, r, c) = wrap_input("", 0, 20);
        assert_eq!(rows, vec![String::new()]);
        assert_eq!((r, c), (0, 0));
    }

    #[test]
    fn height_grows_with_content_then_caps() {
        let inner = input_inner_width(80); // 76
        // One row of content → 1 content row + 2 borders.
        assert_eq!(input_height_for("short", inner, 80), 3);
        // A paste far exceeding the cap clamps to MAX_INPUT_ROWS + borders.
        let huge = "x".repeat(inner * (MAX_INPUT_ROWS + 5));
        assert_eq!(input_height_for(&huge, inner, 80), MAX_INPUT_ROWS as u16 + 2);
    }

    // Mirror of input_height without building an App: wraps the raw insert
    // buffer (leading space + text) at the given inner width.
    fn input_height_for(input: &str, _inner: usize, full_width: u16) -> u16 {
        let content = format!(" {input}");
        let cursor = 1 + input.chars().count();
        let (rows, _, _) = wrap_input(&content, cursor, input_inner_width(full_width));
        rows.len().clamp(1, MAX_INPUT_ROWS) as u16 + 2
    }
}
