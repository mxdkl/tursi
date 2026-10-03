//! Frame layout (§2): header / transcript / input / status, overlays on top.

use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, Paragraph};

use super::{App, Mode, Overlay};

pub fn draw(frame: &mut Frame, app: &mut App) {
    let full = frame.area();
    // The input box grows with wrapped content so long input never runs off
    // the edge; capped so a big paste can't swallow the transcript.
    let input_h = input_height(app, full.width);
    let [header_area, transcript_area, input_area, status_area] = Layout::vertical([
        Constraint::Length(1),
        Constraint::Min(3),
        Constraint::Length(input_h),
        Constraint::Length(1),
    ])
    .areas(full);

    header(frame, header_area, app);
    transcript(frame, transcript_area, app);
    input_bar(frame, input_area, app);
    status_bar(frame, status_area, app);
    if app.overlay.is_some() {
        overlay(frame, full, app);
    }
}

fn header(frame: &mut Frame, area: Rect, app: &App) {
    let project = app
        .session
        .project
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("?");
    let mut spans = vec![
        Span::styled(format!(" {project} "), Style::default().add_modifier(Modifier::BOLD)),
        Span::raw("│ "),
    ];
    if app.plan_active {
        spans.push(Span::styled("PLAN ", Style::default().fg(Color::Magenta)));
    }
    if app.afk {
        spans.push(Span::styled("AFK ", Style::default().fg(Color::Cyan)));
    }
    spans.push(Span::raw(format!(
        "│ {} │ Sess ${:.2} │ Mo ${:.2} │ ",
        app.model, app.session_usd, app.month_usd
    )));
    // Context-window gauge: percent full + rough token size, colored as it
    // approaches the compaction threshold (§8).
    let pct = if app.ctx_window > 0 {
        (app.ctx_used as f64 / app.ctx_window as f64 * 100.0).round() as u64
    } else {
        0
    };
    let ctx_color = match pct {
        p if p >= 75 => Color::Red,
        p if p >= 60 => Color::Yellow,
        _ => Color::DarkGray,
    };
    spans.push(Span::styled(
        format!("ctx {pct}% ({}k)", app.ctx_used / 1000),
        Style::default().fg(ctx_color),
    ));
    frame.render_widget(Paragraph::new(Line::from(spans)).style(Style::default().bg(Color::Black)), area);
}

/// Bottom-anchored scrollback: scroll == 0 means "latest" (newbbs model) —
/// new lines append at the anchor and the view follows unless scrolled away.
fn transcript(frame: &mut Frame, area: Rect, app: &mut App) {
    let height = area.height as usize;
    app.view_height = height;
    let width = area.width.saturating_sub(1) as usize;
    let mut lines = super::transcript::render(&app.transcript, width, app.verbose);
    if !app.partial.trim().is_empty() {
        // The block being streamed, rendered as if already closed.
        let live = [super::transcript::Entry::Agent(app.partial.trim_end().to_string())];
        lines.extend(super::transcript::render(&live, width, app.verbose));
    }
    app.line_count = lines.len();
    let max_scroll = lines.len().saturating_sub(height);
    app.scroll = app.scroll.min(max_scroll);
    let end = lines.len() - app.scroll;
    let start = end.saturating_sub(height);
    frame.render_widget(Paragraph::new(lines[start..end].to_vec()).block(Block::default().padding(ratatui::widgets::Padding::left(1))), area);
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

/// The active buffer as it's shown, with the cursor's char index into it. One
/// leading space (" text"); command mode leads with " :" — so ":help" sits a
/// space in from the border.
fn input_display(app: &App) -> (String, usize) {
    match app.mode {
        Mode::Command => (format!(" :{}", app.command), 2 + app.command.chars().count()),
        _ => (format!(" {}", app.input), 1 + app.cursor),
    }
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
    let (title, border) = match app.mode {
        Mode::Insert => (" INSERT ", Style::default().fg(Color::Green)),
        Mode::Command => (" : ", Style::default().fg(Color::Yellow)),
        Mode::Normal => (" i to type ", Style::default().fg(Color::DarkGray)),
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
    let shown: Vec<Line> = rows[first..(first + visible).min(rows.len())]
        .iter()
        .map(|r| Line::raw(r.clone()))
        .collect();
    frame.render_widget(
        Paragraph::new(shown).block(Block::default().borders(Borders::ALL).title(title).border_style(border)),
        box_area,
    );
    // Cursor: +1 for the left border; the leading space/colon is already inside
    // `content`, so it's counted in cur_col.
    if app.mode != Mode::Normal {
        let x = box_area.x + 1 + cur_col as u16;
        let y = box_area.y + 1 + (cur_row - first) as u16;
        frame.set_cursor_position((x, y));
    }
}

fn status_bar(frame: &mut Frame, area: Rect, app: &App) {
    let (mode, color) = match app.mode {
        Mode::Normal => ("-- NORMAL --", Color::Blue),
        Mode::Insert => ("-- INSERT --", Color::Green),
        Mode::Command => ("-- COMMAND --", Color::Yellow),
    };
    let mut spans = vec![Span::styled(mode, Style::default().fg(color).add_modifier(Modifier::BOLD))];
    if app.task_running {
        spans.push(Span::styled("  ⋯ running (Esc interrupts)", Style::default().fg(Color::Yellow)));
    }
    if let Some((filled, total)) = app.stubs {
        spans.push(Span::styled(
            format!("  stubs {filled}/{total}"),
            Style::default().fg(Color::Magenta),
        ));
    }
    if !app.monitors.is_empty() {
        let list: Vec<String> = app.monitors.iter().map(|(id, label)| format!("{id} {label}")).collect();
        spans.push(Span::styled(format!("  ⏱ {}", list.join(", ")), Style::default().fg(Color::Yellow)));
    }
    if let Some(status) = &app.status {
        spans.push(Span::raw("  "));
        let style = if status.starts_with('✗') {
            Style::default().fg(Color::Red)
        } else {
            Style::default().fg(Color::DarkGray)
        };
        spans.push(Span::styled(status.as_str(), style));
    }
    frame.render_widget(Paragraph::new(Line::from(spans)), area);
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
NORMAL   j/k scroll · gg/G top/bottom · PgUp/PgDn page · Ctrl+O expand results
         i/a insert · : command · Esc interrupt (while running)
INSERT   Enter send (steers a running task) · Ctrl+U clear · Ctrl+W del word
COMMAND  :q :help :afk :model [id] :rewind [n]
         :plan [task] :approve :sysinfo :log <pattern>
         :monitors · :monitor stop <id>
OVERLAY  network: y/n · ask: 1-9 or type · Ctrl+C always quits";

#[cfg(test)]
mod tests {
    use super::*;

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
