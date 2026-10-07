//! Key handling, no modes: typing always edits the input, Enter sends it
//! (or runs a `/command`), Esc interrupts a running task. The chat scrolls
//! with PgUp/PgDn; Up/Down recall earlier inputs.

use anyhow::Result;
use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers};

use super::{App, Overlay};
use crate::bus::ApprovalReply;

pub async fn handle(app: &mut App, key: KeyEvent) -> Result<()> {
    // Windows sends key-release events too; only act on presses.
    if key.kind == KeyEventKind::Release {
        return Ok(());
    }
    let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
    // Ctrl+C clears a half-typed line; otherwise it always quits, in every
    // state, so the session can never be trapped.
    if key.code == KeyCode::Char('c') && ctrl {
        if app.overlay.is_none() && !app.input.is_empty() {
            clear(app);
        } else {
            app.quit();
        }
        return Ok(());
    }
    if app.overlay.is_some() {
        return overlay(app, key).await;
    }
    let page = app.view_height.saturating_sub(1).max(1);
    match key.code {
        KeyCode::Enter => submit(app).await?,
        KeyCode::Esc => {
            if app.task_running {
                app.interrupt();
            } else {
                clear(app);
                app.status = None;
                app.scroll = 0;
            }
        }
        KeyCode::Char('d') if ctrl && app.input.is_empty() => app.quit(),
        KeyCode::Char('u') if ctrl => clear(app),
        KeyCode::Char('w') if ctrl => delete_word(app),
        KeyCode::Char('a') if ctrl => app.cursor = 0,
        KeyCode::Char('e') if ctrl => app.cursor = app.input.chars().count(),
        KeyCode::Char('k') if ctrl => {
            let at = byte_index(&app.input, app.cursor);
            app.input.truncate(at);
        }
        KeyCode::Char(c) if !ctrl && !key.modifiers.contains(KeyModifiers::ALT) => {
            let at = byte_index(&app.input, app.cursor);
            app.input.insert(at, c);
            app.cursor += 1;
            app.history_pos = None;
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
        KeyCode::Home if ctrl => app.scroll = app.line_count.saturating_sub(app.view_height),
        KeyCode::End if ctrl => app.scroll = 0,
        KeyCode::Home => app.cursor = 0,
        KeyCode::End => app.cursor = app.input.chars().count(),
        KeyCode::Up => recall(app, -1),
        KeyCode::Down => recall(app, 1),
        KeyCode::PageUp => app.scroll += page,
        KeyCode::PageDown => app.scroll = app.scroll.saturating_sub(page),
        _ => {}
    }
    Ok(())
}

fn clear(app: &mut App) {
    app.input.clear();
    app.cursor = 0;
    app.history_pos = None;
}

/// Enter: a `/command` if the first word names one, otherwise a message
/// (so a path like `/home/me/notes.md fix this` still goes to the lead).
async fn submit(app: &mut App) -> Result<()> {
    let text = app.input.trim().to_string();
    if text.is_empty() {
        return Ok(());
    }
    if app.history.last() != Some(&text) {
        app.history.push(text.clone());
    }
    app.history_pos = None;
    if let Some(line) = text.strip_prefix('/') {
        let verb = line.split_whitespace().next().unwrap_or("");
        if super::command::is_command(verb) {
            clear(app);
            if let Err(e) = super::command::run(app, line).await {
                app.set_error(format!("{e:#}"));
            }
            return Ok(());
        }
    }
    app.send_message().await
}

/// Up/Down through earlier inputs; past the newest returns to an empty line.
fn recall(app: &mut App, step: isize) {
    if app.history.is_empty() {
        return;
    }
    let last = app.history.len() - 1;
    let next = match (app.history_pos, step < 0) {
        (None, true) => Some(last),
        (None, false) => None,
        (Some(0), true) => Some(0),
        (Some(i), true) => Some(i - 1),
        (Some(i), false) if i >= last => None,
        (Some(i), false) => Some(i + 1),
    };
    app.history_pos = next;
    app.input = next.map(|i| app.history[i].clone()).unwrap_or_default();
    app.cursor = app.input.chars().count();
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

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    async fn typed(app: &mut App, text: &str) {
        for c in text.chars() {
            handle(app, press(c)).await.unwrap();
        }
    }

    #[tokio::test]
    async fn typing_goes_straight_to_the_input() {
        let (mut app, ..) = testui::app("keys-mb");
        typed(&mut app, "héllo").await;
        handle(&mut app, key(KeyCode::Backspace)).await.unwrap();
        assert_eq!(app.input, "héll");
        handle(&mut app, KeyEvent::new(KeyCode::Char('w'), KeyModifiers::CONTROL)).await.unwrap();
        assert_eq!(app.input, "");
        assert_eq!(app.cursor, 0);
        // Former vim keys are just letters now.
        typed(&mut app, "jk:q").await;
        assert_eq!(app.input, "jk:q");
        assert!(!app.quitting);
    }

    #[tokio::test]
    async fn slash_commands_run_and_other_slashes_are_messages() {
        let (mut app, _tx, _steer, _cancel, mut cmd_rx) = testui::app("keys-slash");
        typed(&mut app, "/afk").await;
        handle(&mut app, key(KeyCode::Enter)).await.unwrap();
        assert!(app.afk && app.input.is_empty());
        assert!(matches!(cmd_rx.try_recv().unwrap(), crate::agent::Command::SetAfk(true)));
        typed(&mut app, "/home/me/notes.md summarize this").await;
        handle(&mut app, key(KeyCode::Enter)).await.unwrap();
        assert!(matches!(cmd_rx.try_recv().unwrap(), crate::agent::Command::Task(t) if t.starts_with("/home/me")));
        // Up recalls what was sent, newest first.
        handle(&mut app, key(KeyCode::Up)).await.unwrap();
        assert_eq!(app.input, "/home/me/notes.md summarize this");
        handle(&mut app, key(KeyCode::Up)).await.unwrap();
        assert_eq!(app.input, "/afk");
        handle(&mut app, key(KeyCode::Down)).await.unwrap();
        handle(&mut app, key(KeyCode::Down)).await.unwrap();
        assert_eq!(app.input, "");
    }

    #[tokio::test]
    async fn esc_interrupts_only_while_a_task_runs() {
        let (mut app, _tx, _steer, cancel_rx, _cmd) = testui::app("keys-esc");
        handle(&mut app, key(KeyCode::Esc)).await.unwrap();
        assert!(!*cancel_rx.borrow(), "idle Esc never cancels");
        app.task_running = true;
        handle(&mut app, key(KeyCode::Esc)).await.unwrap();
        assert!(*cancel_rx.borrow(), "running Esc flips the cancel watch");
    }

    #[tokio::test]
    async fn ctrl_c_clears_a_typed_line_before_it_quits() {
        let (mut app, ..) = testui::app("keys-ctrlc");
        let ctrl_c = KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL);
        typed(&mut app, "half a thought").await;
        handle(&mut app, ctrl_c).await.unwrap();
        assert!(app.input.is_empty() && !app.quitting);
        handle(&mut app, ctrl_c).await.unwrap();
        assert!(app.quitting);
    }
}
