//! Modal key handling: Normal to move, Insert to talk, Command for verbs.
//! Lifted from the newbbs pattern — the keymap stays small enough that
//! `:help` fits on one screen.

use anyhow::Result;
use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers};

use super::{App, Mode, Overlay};
use crate::bus::ApprovalReply;

pub async fn handle(app: &mut App, key: KeyEvent) -> Result<()> {
    // Windows sends key-release events too; only act on presses.
    if key.kind == KeyEventKind::Release {
        return Ok(());
    }
    // Always available, in every mode, so the session can never be trapped.
    if key.code == KeyCode::Char('c') && key.modifiers.contains(KeyModifiers::CONTROL) {
        app.quit();
        return Ok(());
    }
    if app.overlay.is_some() {
        return overlay(app, key).await;
    }
    match app.mode {
        Mode::Normal => normal(app, key).await,
        Mode::Insert => insert(app, key).await,
        Mode::Command => command_line(app, key).await,
    }
}

/// j/k/gg/G/paging scroll; i/a → Insert; ':' → Command; Esc interrupts a
/// running task (§5.2), otherwise clears pending/scroll.
async fn normal(app: &mut App, key: KeyEvent) -> Result<()> {
    if let Some('g') = app.pending {
        app.pending = None;
        if key.code == KeyCode::Char('g') {
            app.scroll = app.line_count.saturating_sub(app.view_height);
            return Ok(());
        }
    }
    let page = app.view_height.saturating_sub(1).max(1);
    if key.code == KeyCode::Char('o') && key.modifiers.contains(KeyModifiers::CONTROL) {
        app.verbose = !app.verbose;
        app.set_status(if app.verbose { "showing full tool results" } else { "tool results collapsed" });
        return Ok(());
    }
    match key.code {
        KeyCode::Char('i') | KeyCode::Char('a') => {
            app.mode = Mode::Insert;
            if key.code == KeyCode::Char('a') {
                app.cursor = app.input.chars().count();
            }
        }
        KeyCode::Char(':') => {
            app.mode = Mode::Command;
            app.command.clear();
        }
        KeyCode::Char('g') => app.pending = Some('g'),
        KeyCode::Char('G') => app.scroll = 0,
        KeyCode::Char('j') | KeyCode::Down => app.scroll = app.scroll.saturating_sub(1),
        KeyCode::Char('k') | KeyCode::Up => app.scroll += 1,
        KeyCode::PageDown => app.scroll = app.scroll.saturating_sub(page),
        KeyCode::PageUp => app.scroll += page,
        KeyCode::Esc => {
            if app.task_running {
                app.interrupt();
            } else {
                app.pending = None;
                app.scroll = 0;
            }
        }
        _ => {}
    }
    Ok(())
}

/// Char-indexed editing with a byte_index helper (multi-byte-safe); Ctrl+U
/// clears, Ctrl+W deletes a word; Enter submits and returns to Normal.
async fn insert(app: &mut App, key: KeyEvent) -> Result<()> {
    let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
    match key.code {
        KeyCode::Esc => app.mode = Mode::Normal,
        KeyCode::Enter => {
            app.send_message().await?;
            // Back to Normal so j/k work immediately (newbbs rule).
            app.mode = Mode::Normal;
        }
        KeyCode::Char('u') if ctrl => {
            app.input.clear();
            app.cursor = 0;
        }
        KeyCode::Char('w') if ctrl => delete_word(app),
        KeyCode::Char(c) => {
            let at = byte_index(&app.input, app.cursor);
            app.input.insert(at, c);
            app.cursor += 1;
        }
        KeyCode::Backspace => {
            if app.cursor > 0 {
                let start = byte_index(&app.input, app.cursor - 1);
                let end = byte_index(&app.input, app.cursor);
                app.input.replace_range(start..end, "");
                app.cursor -= 1;
            }
        }
        KeyCode::Delete => {
            if app.cursor < app.input.chars().count() {
                let start = byte_index(&app.input, app.cursor);
                let end = byte_index(&app.input, app.cursor + 1);
                app.input.replace_range(start..end, "");
            }
        }
        KeyCode::Left => app.cursor = app.cursor.saturating_sub(1),
        KeyCode::Right => app.cursor = (app.cursor + 1).min(app.input.chars().count()),
        KeyCode::Home => app.cursor = 0,
        KeyCode::End => app.cursor = app.input.chars().count(),
        _ => {}
    }
    Ok(())
}

/// Backspace past empty exits to Normal; Enter runs the verb; failures go to
/// the status line, never tear down (newbbs rule).
async fn command_line(app: &mut App, key: KeyEvent) -> Result<()> {
    match key.code {
        KeyCode::Esc => {
            app.mode = Mode::Normal;
            app.command.clear();
        }
        KeyCode::Enter => {
            let line = std::mem::take(&mut app.command);
            app.mode = Mode::Normal;
            if let Err(e) = super::command::run(app, &line).await {
                app.set_error(format!("{e:#}"));
            }
        }
        KeyCode::Backspace => {
            if app.command.pop().is_none() {
                app.mode = Mode::Normal;
            }
        }
        KeyCode::Char(c) => app.command.push(c),
        _ => {}
    }
    Ok(())
}

/// Approval (network): y / n (reason prompt); Ask: digits pick options, text
/// answers; Help/SysInfo: any key closes.
async fn overlay(app: &mut App, key: KeyEvent) -> Result<()> {
    match app.overlay.take() {
        Some(Overlay::Approval(request)) => match key.code {
            KeyCode::Char('y') | KeyCode::Enter => {
                let _ = request.reply.send(ApprovalReply::Approve);
            }
            KeyCode::Char('n') => {
                app.overlay = Some(Overlay::RejectReason(request, String::new()));
            }
            KeyCode::Esc => {
                let _ = request.reply.send(ApprovalReply::Reject { reason: None });
            }
            _ => app.overlay = Some(Overlay::Approval(request)),
        },
        Some(Overlay::RejectReason(request, mut reason)) => match key.code {
            KeyCode::Enter => {
                let reason = reason.trim().to_string();
                let _ = request.reply.send(ApprovalReply::Reject {
                    reason: if reason.is_empty() { None } else { Some(reason) },
                });
            }
            KeyCode::Esc => {
                let _ = request.reply.send(ApprovalReply::Reject { reason: None });
            }
            KeyCode::Backspace => {
                reason.pop();
                app.overlay = Some(Overlay::RejectReason(request, reason));
            }
            KeyCode::Char(c) => {
                reason.push(c);
                app.overlay = Some(Overlay::RejectReason(request, reason));
            }
            _ => app.overlay = Some(Overlay::RejectReason(request, reason)),
        },
        Some(Overlay::Ask(request, mut buffer)) => match key.code {
            KeyCode::Char(d @ '1'..='9') if buffer.is_empty() => {
                let index = d as usize - '1' as usize;
                match request.options.get(index).cloned() {
                    Some(option) => {
                        let _ = request.reply.send(option);
                    }
                    None => app.overlay = Some(Overlay::Ask(request, buffer)),
                }
            }
            KeyCode::Enter if !buffer.trim().is_empty() => {
                let _ = request.reply.send(buffer.trim().to_string());
            }
            KeyCode::Esc => {
                let _ = request.reply.send("(dismissed without an answer)".to_string());
            }
            KeyCode::Backspace => {
                buffer.pop();
                app.overlay = Some(Overlay::Ask(request, buffer));
            }
            KeyCode::Char(c) => {
                buffer.push(c);
                app.overlay = Some(Overlay::Ask(request, buffer));
            }
            _ => app.overlay = Some(Overlay::Ask(request, buffer)),
        },
        Some(Overlay::Help) | Some(Overlay::SysInfo) => {}
        None => {}
    }
    // An answered overlay resumes the §5.6 machine.
    if app.overlay.is_none() {
        let state = if app.task_running {
            crate::session::State::Running
        } else {
            crate::session::State::Idle
        };
        let _ = app.session.transition(state);
    }
    Ok(())
}

/// Character index → byte index, so multi-byte input edits safely.
fn byte_index(text: &str, chars: usize) -> usize {
    text.char_indices().nth(chars).map(|(i, _)| i).unwrap_or(text.len())
}

fn delete_word(app: &mut App) {
    let chars: Vec<char> = app.input.chars().collect();
    let mut at = app.cursor;
    while at > 0 && chars[at - 1].is_whitespace() {
        at -= 1;
    }
    while at > 0 && !chars[at - 1].is_whitespace() {
        at -= 1;
    }
    let start = byte_index(&app.input, at);
    let end = byte_index(&app.input, app.cursor);
    app.input.replace_range(start..end, "");
    app.cursor = at;
}

#[cfg(test)]
pub(crate) fn press(c: char) -> KeyEvent {
    KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ui::testui;

    #[tokio::test]
    async fn insert_mode_edits_multibyte_text_safely() {
        let (mut app, ..) = testui::app("keys-mb");
        handle(&mut app, press('i')).await.unwrap();
        for c in "héllo".chars() {
            handle(&mut app, press(c)).await.unwrap();
        }
        handle(&mut app, KeyEvent::new(KeyCode::Backspace, KeyModifiers::NONE)).await.unwrap();
        assert_eq!(app.input, "héll");
        handle(&mut app, KeyEvent::new(KeyCode::Char('w'), KeyModifiers::CONTROL)).await.unwrap();
        assert_eq!(app.input, "");
        assert_eq!(app.cursor, 0);
    }

    #[tokio::test]
    async fn command_mode_backspace_past_empty_exits_to_normal() {
        let (mut app, ..) = testui::app("keys-cmd");
        handle(&mut app, press(':')).await.unwrap();
        assert_eq!(app.mode, crate::ui::Mode::Command);
        handle(&mut app, press('q')).await.unwrap();
        handle(&mut app, KeyEvent::new(KeyCode::Backspace, KeyModifiers::NONE)).await.unwrap();
        assert_eq!(app.command, "");
        handle(&mut app, KeyEvent::new(KeyCode::Backspace, KeyModifiers::NONE)).await.unwrap();
        assert_eq!(app.mode, crate::ui::Mode::Normal);
    }

    #[tokio::test]
    async fn esc_interrupts_only_while_a_task_runs() {
        let (mut app, _tx, _steer, cancel_rx, _cmd) = testui::app("keys-esc");
        handle(&mut app, KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE)).await.unwrap();
        assert!(!*cancel_rx.borrow(), "idle Esc never cancels");
        app.task_running = true;
        handle(&mut app, KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE)).await.unwrap();
        assert!(*cancel_rx.borrow(), "running Esc flips the cancel watch");
    }
}
