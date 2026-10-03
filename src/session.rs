//! Session lifecycle, the §5.6 state machine, and the crash-resume journal.

use anyhow::{Context, Result};
use chrono::Utc;
use std::io::Write;
use std::path::{Path, PathBuf};
use uuid::Uuid;

use crate::bus::AgentId;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum State {
    Idle,
    Running,
    AwaitingApproval,
    AwaitingUser,
    Verifying,
}

pub struct Session {
    pub id: Uuid,
    pub state: State,
    pub project: PathBuf,
    /// Reattached to an earlier session (`--resume`): the loop restores its
    /// transcript, the UI replays it.
    pub resumed: bool,
    journal: PathBuf,
}

/// One row of `tursi --sessions`.
pub struct Summary {
    pub id: Uuid,
    pub started: String,
    pub closed: bool,
    /// The first instruction, for recognition.
    pub first_prompt: String,
    pub messages: usize,
}

impl Session {
    /// Open fresh, or reattach: `resume` is `"latest"` (the most recent
    /// session of this project) or an id prefix. UUIDv7 ids sort by time.
    pub fn open_or_resume(project: &Path, resume: Option<&str>) -> Result<Session> {
        let dir = project.join(".tursi/sessions");
        std::fs::create_dir_all(&dir)?;
        if let Some(wanted) = resume {
            let ids = session_ids(&dir)?;
            let found = match wanted {
                "latest" | "" => ids.last().copied(),
                prefix => ids.iter().rev().find(|id| id.to_string().starts_with(prefix)).copied(),
            };
            let Some(id) = found else {
                anyhow::bail!(
                    "no session to resume{} — `tursi --sessions` lists them",
                    if wanted == "latest" { String::new() } else { format!(" matching {wanted:?}") }
                );
            };
            let journal = dir.join(format!("{id}.jsonl"));
            let session = Session { id, state: State::Idle, project: project.to_path_buf(), resumed: true, journal };
            session.journal(crate::bus::ROOT, "resumed")?;
            return Ok(session);
        }
        let id = Uuid::now_v7();
        let journal = dir.join(format!("{id}.jsonl"));
        let session = Session { id, state: State::Idle, project: project.to_path_buf(), resumed: false, journal };
        session.journal(crate::bus::ROOT, "opened")?;
        Ok(session)
    }

    /// Where a session's transcript is persisted (`agent::AgentLoop::persist`).
    pub fn transcript_path(project: &Path, id: Uuid) -> PathBuf {
        project.join(".tursi/sessions").join(format!("{id}.transcript.json"))
    }

    /// Recent sessions of this project, oldest first.
    pub fn list(project: &Path) -> Result<Vec<Summary>> {
        let dir = project.join(".tursi/sessions");
        let mut out = Vec::new();
        for id in session_ids(&dir).unwrap_or_default() {
            let journal = std::fs::read_to_string(dir.join(format!("{id}.jsonl"))).unwrap_or_default();
            let line = |l: &str| serde_json::from_str::<serde_json::Value>(l).ok();
            let started = journal.lines().next().and_then(line).and_then(|v| v.get("ts")?.as_str().map(|s| s.chars().take(16).collect::<String>().replace('T', " "))).unwrap_or_default();
            let closed = journal.lines().last().and_then(line).and_then(|v| Some(v.get("event")?.as_str()? == "closed")).unwrap_or(false);
            let (first_prompt, messages) = std::fs::read_to_string(Self::transcript_path(project, id))
                .ok()
                .and_then(|t| serde_json::from_str::<serde_json::Value>(&t).ok())
                .and_then(|v| {
                    let msgs = v.get("messages")?.as_array()?;
                    let first = msgs.iter().find_map(|m| m.get("User")?.as_str().map(|s| s.lines().next().unwrap_or("").chars().take(60).collect::<String>()));
                    Some((first.unwrap_or_default(), msgs.len()))
                })
                .unwrap_or_default();
            out.push(Summary { id, started, closed, first_prompt, messages });
        }
        Ok(out)
    }

    /// Transition + journal append; a same-state transition is a no-op.
    pub fn transition(&mut self, to: State) -> Result<()> {
        if to == self.state {
            return Ok(());
        }
        self.journal(crate::bus::ROOT, &format!("state {:?} -> {to:?}", self.state))?;
        self.state = to;
        Ok(())
    }

    /// One JSONL line per event (agent id per §5.7): state changes, task
    /// starts/ends.
    fn journal(&self, agent: AgentId, event: &str) -> Result<()> {
        let mut file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.journal)
            .with_context(|| format!("journal {}", self.journal.display()))?;
        writeln!(
            file,
            "{}",
            serde_json::json!({"ts": Utc::now(), "agent": agent.0, "event": event})
        )?;
        Ok(())
    }

    /// Clean close: the final journal line is what marks a session resumable
    /// or not.
    pub fn close(self) -> Result<()> {
        self.journal(crate::bus::ROOT, "closed")
    }
}

/// Every session id in the directory, oldest first.
fn session_ids(dir: &Path) -> Result<Vec<Uuid>> {
    let mut ids: Vec<Uuid> = std::fs::read_dir(dir)?
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.extension().and_then(|e| e.to_str()) == Some("jsonl"))
        .filter_map(|p| Uuid::parse_str(p.file_stem()?.to_str()?).ok())
        .collect();
    ids.sort();
    Ok(ids)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tools::testutil;

    #[test]
    fn resume_picks_the_latest_session_or_an_id_prefix() {
        let dir = testutil::tmp("session");
        assert!(Session::open_or_resume(&dir, Some("latest")).is_err(), "nothing to resume yet");
        let first = Session::open_or_resume(&dir, None).unwrap();
        let first_id = first.id;
        first.close().unwrap();
        // UUIDv7 orders by millisecond: make sure the second is later.
        std::thread::sleep(std::time::Duration::from_millis(3));
        let second = Session::open_or_resume(&dir, None).unwrap();
        let second_id = second.id;
        drop(second);

        let list = Session::list(&dir).unwrap();
        assert_eq!(list.len(), 2);
        assert!(list[0].closed && list[0].id == first_id && !list[1].closed);

        let latest = Session::open_or_resume(&dir, Some("latest")).unwrap();
        assert_eq!(latest.id, second_id);
        assert!(latest.resumed);
        let by_prefix = Session::open_or_resume(&dir, Some(&first_id.to_string()[..24])).unwrap();
        assert_eq!(by_prefix.id, first_id, "closed sessions resume too");
        assert!(!Session::list(&dir).unwrap()[0].closed, "resuming reopens it");
        assert!(Session::open_or_resume(&dir, Some("ffffffff")).is_err());
    }

    #[test]
    fn transitions_journal_and_same_state_is_a_noop() {
        let dir = testutil::tmp("session-t");
        let mut s = Session::open_or_resume(&dir, None).unwrap();
        s.transition(State::Running).unwrap();
        s.transition(State::Running).unwrap();
        s.transition(State::Verifying).unwrap();
        assert_eq!(s.state, State::Verifying);
        let journal = std::fs::read_to_string(
            dir.join(".tursi/sessions").join(format!("{}.jsonl", s.id)),
        )
        .unwrap();
        assert!(journal.contains("opened"));
        assert_eq!(journal.matches("state ").count(), 2, "no-op not journaled");
        assert!(journal.contains("Running -> Verifying"));
    }
}
