//! Command mode (`:`) — flat verb dispatch, usage-string errors (§2).

use anyhow::Result;

use super::{App, Overlay};
use crate::agent;

pub async fn run(app: &mut App, line: &str) -> Result<()> {
    let line = line.trim();
    if line.is_empty() {
        return Ok(());
    }
    let (verb, rest) = match line.split_once(char::is_whitespace) {
        Some((verb, rest)) => (verb, rest.trim()),
        None => (line, ""),
    };
    match verb {
        "q" | "quit" => app.quit(),
        "help" | "h" => app.overlay = Some(Overlay::Help),
        "model" => model(app, rest),
        "rewind" => rewind(app, rest),
        "plan" => plan(app, rest).await,
        "approve" => approve(app),
        "afk" => afk(app),
        "sysinfo" => app.overlay = Some(Overlay::SysInfo),
        "log" => log(app, rest),
        "monitors" => monitors(app),
        "monitor" => monitor(app, rest),
        other => app.set_error(format!("unknown command :{other} — try :help")),
    }
    Ok(())
}

/// Control commands ride try_send: the agent channel is small and a full
/// queue means the user is spamming faster than the agent drains — tell them.
fn send(app: &mut App, command: agent::Command) -> bool {
    match app.cmd.try_send(command) {
        Ok(()) => true,
        Err(_) => {
            app.set_error("agent is busy — command dropped, try again");
            false
        }
    }
}

/// `:model <id>` pins the model; `:model` alone unpins (§7).
fn model(app: &mut App, rest: &str) {
    let pin = if rest.is_empty() { None } else { Some(rest.to_string()) };
    if send(app, agent::Command::Pin(pin.clone())) {
        match pin {
            Some(id) => {
                app.model = id.clone();
                app.set_status(format!("model pinned to {id}"));
            }
            None => app.set_status("model unpinned — the configured model resumes next task"),
        }
    }
}

/// `:rewind [n]` — restore checkpoints (§3.3).
fn rewind(app: &mut App, rest: &str) {
    let n = if rest.is_empty() { 1 } else {
        match rest.parse() {
            Ok(n) => n,
            Err(_) => return app.set_error("usage: :rewind [n]"),
        }
    };
    send(app, agent::Command::Rewind(n));
}

/// `:plan [task]` — skeleton plan mode; with a task it starts immediately (§5.5).
async fn plan(app: &mut App, rest: &str) {
    if !send(app, agent::Command::PlanEnter) {
        return;
    }
    app.plan_active = true;
    if rest.is_empty() {
        app.set_status("PLAN — next task is a skeleton; :approve when it convinces you");
    } else {
        app.transcript.push(crate::ui::transcript::Entry::User(format!(":plan {rest}")));
        app.task_running = true;
        app.task_started = Some(std::time::Instant::now());
        app.task_start_usd = app.session_usd;
        let _ = app.cmd.send(agent::Command::Task(rest.to_string())).await;
    }
}

/// `:approve` — typecheck gate, then fill-in (§5.5).
fn approve(app: &mut App) {
    if send(app, agent::Command::PlanApprove) {
        app.plan_active = false;
        app.set_status("running the typecheck gate…");
    }
}

/// `:afk` — unattended mode: ask_user auto-resolves (§4.4).
fn afk(app: &mut App) {
    let next = !app.afk;
    if send(app, agent::Command::SetAfk(next)) {
        app.afk = next;
        app.set_status(if next {
            "AFK — ask_user auto-resolves, verify gates turn ends (§5.3)"
        } else {
            "attended — ask_user blocks again"
        });
    }
}

/// `:monitors` — what's armed (the status bar shows it too).
fn monitors(app: &mut App) {
    if app.monitors.is_empty() {
        return app.set_status("no monitors armed");
    }
    let list: Vec<String> = app.monitors.iter().map(|(id, label)| format!("{id} {label}")).collect();
    app.set_status(format!("monitors: {}", list.join(", ")));
}

/// `:monitor stop <id>`.
fn monitor(app: &mut App, rest: &str) {
    match rest.split_once(char::is_whitespace) {
        Some(("stop", id)) if !id.trim().is_empty() => {
            send(app, agent::Command::MonitorStop(id.trim().to_string()));
        }
        _ => app.set_error("usage: :monitor stop <id>"),
    }
}

/// `:log <pattern>` — grep debug.log into the transcript (§2).
fn log(app: &mut App, rest: &str) {
    if rest.is_empty() {
        return app.set_error("usage: :log <pattern>");
    }
    let path = app.session.project.join(".tursi/debug.log");
    let content = std::fs::read_to_string(&path).unwrap_or_default();
    let matches: Vec<&str> = content.lines().filter(|l| l.contains(rest)).collect();
    if matches.is_empty() {
        return app.set_status("no matches in debug.log");
    }
    let shown = matches.len().min(20);
    let mut note = format!("── :log {rest} ({} of {} matches)", shown, matches.len());
    for line in matches.iter().rev().take(20).rev() {
        note.push_str(&format!("\n  {line}"));
    }
    app.transcript.push(crate::ui::transcript::Entry::Note(note));
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ui::testui;

    #[tokio::test]
    async fn verbs_dispatch_to_agent_commands_and_update_badges() {
        let (mut app, _tx, _steer, _cancel, mut cmd_rx) = testui::app("cmd-verbs");
        run(&mut app, "afk").await.unwrap();
        assert!(app.afk);
        assert!(matches!(cmd_rx.try_recv().unwrap(), agent::Command::SetAfk(true)));

        run(&mut app, "model deepseek/deepseek-chat").await.unwrap();
        assert_eq!(app.model, "deepseek/deepseek-chat");
        assert!(matches!(cmd_rx.try_recv().unwrap(), agent::Command::Pin(Some(_))));

        run(&mut app, "plan build the thing").await.unwrap();
        assert!(app.plan_active && app.task_running);
        assert!(matches!(cmd_rx.try_recv().unwrap(), agent::Command::PlanEnter));
        assert!(matches!(cmd_rx.try_recv().unwrap(), agent::Command::Task(t) if t == "build the thing"));

        run(&mut app, "bogus").await.unwrap();
        assert!(app.status.as_deref().unwrap_or("").contains("unknown command"));
    }
}
