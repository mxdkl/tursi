//! Session lifecycle and the §5.6 state machine. Every transition is a
//! `session` event in the project ledger (`crate::ledger`), which is also
//! what crash-resume and `tursi --sessions` read.

use anyhow::Result;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use uuid::Uuid;

use crate::ledger::{Ledger, What};

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
    /// conversation, the UI replays it.
    pub resumed: bool,
    ledger: Arc<Ledger>,
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
        let ledger = crate::ledger::for_project(project);
        let (id, resumed) = match resume {
            Some(wanted) => {
                let ids = ledger.session_ids();
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
                (id, true)
            }
            None => (Uuid::now_v7(), false),
        };
        ledger.set_session(id);
        crate::ledger::set_primary(ledger.clone());
        let session = Session { id, state: State::Idle, project: project.to_path_buf(), resumed, ledger };
        session.journal(if resumed { "resumed" } else { "opened" });
        Ok(session)
    }

    /// Recent sessions of this project, oldest first.
    pub fn list(project: &Path) -> Result<Vec<Summary>> {
        Ok(crate::ledger::for_project(project)
            .sessions()
            .into_iter()
            .map(|s| Summary {
                id: s.id,
                started: s.started.format("%Y-%m-%d %H:%M").to_string(),
                closed: s.closed,
                first_prompt: s.first_prompt,
                messages: s.messages,
            })
            .collect())
    }

    /// Transition + ledger event; a same-state transition is a no-op.
    pub fn transition(&mut self, to: State) -> Result<()> {
        if to == self.state {
            return Ok(());
        }
        self.journal(&format!("state {:?} -> {to:?}", self.state));
        self.state = to;
        Ok(())
    }

    fn journal(&self, event: &str) {
        self.ledger.append(crate::bus::ROOT, What::Session { event: event.to_string() });
    }

    /// Clean close: the final session event is what marks a session
    /// resumable or not.
    pub fn close(self) -> Result<()> {
        self.journal("closed");
        Ok(())
    }
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
        let journal = crate::ledger::raw(&dir);
        assert!(journal.contains("opened"));
        assert_eq!(journal.matches("state ").count(), 2, "no-op not journaled");
        assert!(journal.contains("Running -> Verifying"));
    }
}
