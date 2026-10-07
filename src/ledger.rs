//! The project ledger (§1, §9): one append-only file, `.tursi/ledger.jsonl`,
//! records everything that happens in a project — session lifecycle, every
//! agent's conversation, full tool output, file changes (tool- and shell-made),
//! and the harness's own diagnostics — one JSON event per line, each with
//! its time, session and agent. Large payloads (file versions, long output)
//! go to `.tursi/blobs/`, named by their blake3 hash and written once; the
//! line holds the hash. An event's id is its byte offset (`log#N`), stable
//! because the file only grows. Appends from other tursi processes on the
//! same project are serialized by an OFD lock the harness holds for the one
//! write and nothing else (no model ever holds it).
//!
//! Handles are cached per project path, like the files they wrap, so tools
//! that know only their project write to the same ledger as the loop.

use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::fs::File;
use std::io::{BufRead, BufReader, Seek, SeekFrom, Write};
use std::os::fd::AsRawFd;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};
use uuid::Uuid;

use crate::api::Message;
use crate::bus::AgentId;

/// Outputs up to this size stay on the line; larger ones become blobs.
const INLINE_OUTPUT: usize = 16 * 1024;

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Event {
    pub ts: DateTime<Utc>,
    pub session: Uuid,
    pub agent: u32,
    #[serde(flatten)]
    pub what: What,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum What {
    /// opened, resumed, closed, `state Idle -> Running`.
    Session { event: String },
    /// One message appended to the agent's conversation.
    Message { task: u32, message: Message },
    /// The whole conversation, written when history was rewritten
    /// (compaction, repair): replay starts from the latest one.
    Snapshot { task: u32, goal: Option<String>, messages: Vec<Message> },
    /// `/goal` set or cleared.
    Goal { condition: Option<String> },
    /// A tool's full output (redacted); `text` inline or `blob` by hash.
    Output { tool: String, text: Option<String>, blob: Option<String>, lines: usize },
    /// A file change: content hashes before and after (None: absent), how
    /// (`edit`, `write`, `shell`, `external`), the shell command, and other
    /// agents whose commands ran at the same time.
    File {
        task: u32,
        path: PathBuf,
        before: Option<String>,
        after: Option<String>,
        via: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        command: Option<String>,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        overlap: Vec<u32>,
        /// How `after` is stored: an exact line edit script from `before`
        /// (inline, or `diff_blob` when big); neither means a whole blob.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        diff: Option<Vec<crate::diff::Hunk>>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        diff_blob: Option<String>,
        /// `before` lives in git as this object (a tracked file's first change).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        base_git: Option<String>,
    },
    /// A harness diagnostic.
    Trace { level: String, target: String, message: String },
}

/// What a conversation replays to.
pub struct Conversation {
    pub task: u32,
    pub goal: Option<String>,
    pub messages: Vec<Message>,
}

/// One row of `tursi --sessions`.
pub struct SessionSummary {
    pub id: Uuid,
    pub started: DateTime<Utc>,
    pub closed: bool,
    pub first_prompt: String,
    pub messages: usize,
}

pub struct Ledger {
    path: PathBuf,
    blobs: PathBuf,
    file: Mutex<Option<File>>,
    session: Mutex<Uuid>,
}

static LEDGERS: OnceLock<Mutex<HashMap<PathBuf, Arc<Ledger>>>> = OnceLock::new();
/// The ledger of the session this process serves: diagnostics go here.
static PRIMARY: OnceLock<Arc<Ledger>> = OnceLock::new();
/// Diagnostics from before the session opened (config warnings).
static EARLY: Mutex<Vec<(DateTime<Utc>, String, String, String)>> = Mutex::new(Vec::new());

/// The ledger of `project`, shared by everything in the process.
pub fn for_project(project: &Path) -> Arc<Ledger> {
    let map = LEDGERS.get_or_init(|| Mutex::new(HashMap::new()));
    let mut map = map.lock().unwrap();
    map.entry(project.to_path_buf())
        .or_insert_with(|| {
            Arc::new(Ledger {
                path: project.join(".tursi/ledger.jsonl"),
                blobs: project.join(".tursi/blobs"),
                file: Mutex::new(None),
                session: Mutex::new(Uuid::nil()),
            })
        })
        .clone()
}

/// Route diagnostics to this ledger from now on (and flush the early ones).
pub fn set_primary(ledger: Arc<Ledger>) {
    if PRIMARY.set(ledger.clone()).is_ok() {
        for (ts, level, target, message) in std::mem::take(&mut *EARLY.lock().unwrap()) {
            ledger.append_at(ts, AgentId(0), What::Trace { level, target, message });
        }
    }
}

fn ofd_lock(file: &File, kind: i32) -> std::io::Result<()> {
    use nix::libc;
    let mut fl: libc::flock = unsafe { std::mem::zeroed() };
    fl.l_type = kind as i16;
    fl.l_whence = libc::SEEK_SET as i16;
    // Whole file; l_pid must be 0 for OFD locks.
    let r = unsafe { libc::fcntl(file.as_raw_fd(), libc::F_OFD_SETLKW, &fl) };
    if r == 0 { Ok(()) } else { Err(std::io::Error::last_os_error()) }
}

fn render_traces(level: &str, target: &str, message: &str) -> String {
    format!("{level} {target}: {message}")
}

impl Ledger {
    pub fn session(&self) -> Uuid {
        *self.session.lock().unwrap()
    }

    pub fn set_session(&self, id: Uuid) {
        *self.session.lock().unwrap() = id;
    }

    /// Append one event; returns its offset (its id). Bookkeeping never sinks
    /// the work: a failed append is dropped (and never traced, which would
    /// append again).
    pub fn append(&self, agent: AgentId, what: What) -> u64 {
        self.append_at(Utc::now(), agent, what)
    }

    /// Append under an explicit session (an agent loop knows its own).
    pub fn append_for(&self, session: Uuid, agent: AgentId, what: What) -> u64 {
        self.write_event(Event { ts: Utc::now(), session, agent: agent.0, what })
    }

    fn append_at(&self, ts: DateTime<Utc>, agent: AgentId, what: What) -> u64 {
        self.write_event(Event { ts, session: self.session(), agent: agent.0, what })
    }

    fn write_event(&self, event: Event) -> u64 {
        let Ok(mut line) = serde_json::to_string(&event) else { return 0 };
        line.push('\n');
        self.write_line(&line).unwrap_or(0)
    }

    fn write_line(&self, line: &str) -> Result<u64> {
        let mut guard = self.file.lock().unwrap();
        if guard.is_none() {
            if let Some(dir) = self.path.parent() {
                std::fs::create_dir_all(dir)?;
            }
            *guard = Some(std::fs::OpenOptions::new().create(true).append(true).read(true).open(&self.path)?);
        }
        let file = guard.as_mut().unwrap();
        ofd_lock(file, nix::libc::F_WRLCK)?;
        let written = (|| -> std::io::Result<u64> {
            let offset = file.seek(SeekFrom::End(0))?;
            file.write_all(line.as_bytes())?;
            Ok(offset)
        })();
        let _ = ofd_lock(file, nix::libc::F_UNLCK);
        Ok(written?)
    }

    /// Store bytes under their hash (once) and return the hash.
    pub fn put_blob(&self, bytes: &[u8]) -> Result<String> {
        let hash = blake3::hash(bytes).to_hex().to_string();
        let path = self.blob_path(&hash);
        if !path.exists() {
            std::fs::create_dir_all(path.parent().unwrap())?;
            let tmp = path.with_extension(format!("tmp{}", std::process::id()));
            std::fs::write(&tmp, bytes)?;
            std::fs::rename(&tmp, &path)?;
        }
        Ok(hash)
    }

    pub fn blob(&self, hash: &str) -> Result<Vec<u8>> {
        std::fs::read(self.blob_path(hash)).with_context(|| format!("ledger blob {hash}"))
    }

    fn blob_path(&self, hash: &str) -> PathBuf {
        self.blobs.join(&hash[..2.min(hash.len())]).join(hash)
    }

    /// A tool's full output, redacted line by line; returns its log id.
    pub fn output(&self, agent: AgentId, tool: &str, raw: &str) -> u64 {
        let text: String = raw.lines().map(crate::output::redact).collect::<Vec<_>>().join("\n");
        let lines = text.lines().count();
        let (inline, blob) = if text.len() <= INLINE_OUTPUT {
            (Some(text), None)
        } else {
            match self.put_blob(text.as_bytes()) {
                Ok(hash) => (None, Some(hash)),
                Err(_) => (Some(text), None),
            }
        };
        self.append(agent, What::Output { tool: tool.to_string(), text: inline, blob, lines })
    }

    /// Every line from the start, with its offset; `f` returns false to stop.
    pub fn scan(&self, mut f: impl FnMut(u64, &str) -> bool) -> Result<()> {
        let Ok(file) = File::open(&self.path) else { return Ok(()) };
        let mut reader = BufReader::new(file);
        let (mut offset, mut line) = (0u64, String::new());
        loop {
            line.clear();
            let n = reader.read_line(&mut line)?;
            if n == 0 {
                break;
            }
            if line.ends_with('\n') && !f(offset, line.trim_end()) {
                break;
            }
            offset += n as u64;
        }
        Ok(())
    }

    /// Parsed events whose line contains every needle (a cheap prefilter).
    pub fn events(&self, needles: &[&str]) -> Vec<(u64, Event)> {
        let mut out = Vec::new();
        let _ = self.scan(|offset, line| {
            if needles.iter().all(|n| line.contains(n)) {
                if let Ok(e) = serde_json::from_str::<Event>(line) {
                    out.push((offset, e));
                }
            }
            true
        });
        out
    }

    /// The event at `offset`.
    pub fn at(&self, offset: u64) -> Option<Event> {
        let mut file = File::open(&self.path).ok()?;
        file.seek(SeekFrom::Start(offset)).ok()?;
        let mut line = String::new();
        BufReader::new(file).read_line(&mut line).ok()?;
        serde_json::from_str(line.trim_end()).ok()
    }

    /// An agent's conversation in a session: the latest snapshot plus every
    /// message after it.
    pub fn conversation(&self, session: Uuid, agent: AgentId) -> Option<Conversation> {
        let sid = session.to_string();
        let key = format!("\"agent\":{},", agent.0);
        let mut conv: Option<Conversation> = None;
        for (_, e) in self.events(&[sid.as_str(), key.as_str()]) {
            if e.session != session || e.agent != agent.0 {
                continue;
            }
            let c = conv.get_or_insert(Conversation { task: 0, goal: None, messages: Vec::new() });
            match e.what {
                What::Snapshot { task, goal, messages } => *c = Conversation { task, goal, messages },
                What::Message { task, message } => {
                    c.task = task;
                    c.messages.push(message);
                }
                What::Goal { condition } => c.goal = condition,
                _ => {}
            }
        }
        conv
    }

    /// Session ids in the order they were opened.
    pub fn session_ids(&self) -> Vec<Uuid> {
        let mut ids = Vec::new();
        for (_, e) in self.events(&["\"kind\":\"session\"", "\"opened\""]) {
            if matches!(&e.what, What::Session { event } if event == "opened") && !ids.contains(&e.session) {
                ids.push(e.session);
            }
        }
        ids
    }

    pub fn sessions(&self) -> Vec<SessionSummary> {
        let mut rows: Vec<SessionSummary> = Vec::new();
        let mut index: HashMap<Uuid, usize> = HashMap::new();
        let _ = self.scan(|_, line| {
            let interesting = line.contains("\"kind\":\"session\"") || (line.contains("\"agent\":0,") && line.contains("\"kind\":\"message\""));
            if !interesting {
                return true;
            }
            let Ok(e) = serde_json::from_str::<Event>(line) else { return true };
            let i = *index.entry(e.session).or_insert_with(|| {
                rows.push(SessionSummary { id: e.session, started: e.ts, closed: false, first_prompt: String::new(), messages: 0 });
                rows.len() - 1
            });
            match e.what {
                What::Session { event } => rows[i].closed = event == "closed",
                What::Message { message, .. } => {
                    rows[i].messages += 1;
                    if let Message::User(text) = message {
                        if rows[i].first_prompt.is_empty() && !text.starts_with('[') {
                            rows[i].first_prompt = text.lines().next().unwrap_or("").chars().take(60).collect();
                        }
                    }
                }
                _ => {}
            }
            true
        });
        rows
    }

    /// The full text of an output event.
    pub fn output_text(&self, what: &What) -> Option<String> {
        match what {
            What::Output { text: Some(t), .. } => Some(t.clone()),
            What::Output { blob: Some(h), .. } => self.blob(h).ok().map(|b| String::from_utf8_lossy(&b).into_owned()),
            _ => None,
        }
    }

    /// `log_search` (§4): `log#N` fetches that output; anything else greps
    /// every tool output, newest first, with context windows.
    pub fn search(&self, pattern: &str, context: usize, max_matches: usize) -> String {
        if let Some(id) = pattern.trim().strip_prefix("log#").and_then(|n| n.parse::<u64>().ok()) {
            return match self.at(id) {
                Some(e) => match self.output_text(&e.what) {
                    Some(text) => format!("{}\n{}", header(id, &e), crate::output::truncate(&text, 300)),
                    None => format!("log#{id} is not a tool output"),
                },
                None => format!("no event log#{id}"),
            };
        }
        let outputs = self.events(&["\"kind\":\"output\""]);
        let mut out = String::new();
        let mut matches = 0usize;
        for (id, e) in outputs.iter().rev() {
            let Some(text) = self.output_text(&e.what) else { continue };
            let lines: Vec<&str> = text.lines().collect();
            let hits: Vec<usize> = lines.iter().enumerate().filter(|(_, l)| l.contains(pattern)).map(|(i, _)| i).collect();
            if hits.is_empty() {
                continue;
            }
            let mut keep = std::collections::BTreeSet::new();
            for &i in &hits {
                if matches >= max_matches {
                    break;
                }
                matches += 1;
                for j in i.saturating_sub(context)..=(i + context).min(lines.len().saturating_sub(1)) {
                    keep.insert(j);
                }
            }
            out.push_str(&header(*id, e));
            out.push('\n');
            let mut last: Option<usize> = None;
            for &i in &keep {
                if last.is_some_and(|p| i > p + 1) {
                    out.push_str("· · ·\n");
                }
                out.push_str(lines[i]);
                out.push('\n');
                last = Some(i);
            }
            if matches >= max_matches {
                out.push_str(&format!("… more matches — narrow the pattern (showing {max_matches})\n"));
                break;
            }
        }
        if out.is_empty() { "no matches in the session log".to_string() } else { out.trim_end().to_string() }
    }

    /// `/log <pattern>`: the last `limit` matching events, one line each.
    pub fn grep(&self, pattern: &str, limit: usize) -> (Vec<String>, usize) {
        let mut hits: Vec<String> = Vec::new();
        let mut total = 0usize;
        let _ = self.scan(|offset, line| {
            if line.contains(pattern) {
                total += 1;
                if let Ok(e) = serde_json::from_str::<Event>(line) {
                    hits.push(brief(offset, &e));
                    if hits.len() > limit {
                        hits.remove(0);
                    }
                }
            }
            true
        });
        (hits, total)
    }
}

fn header(id: u64, e: &Event) -> String {
    let tool = match &e.what {
        What::Output { tool, .. } => tool.as_str(),
        _ => "?",
    };
    format!("── log#{id} agent={} tool={tool} ──", e.agent)
}

/// One event as a line for `/log`.
fn brief(id: u64, e: &Event) -> String {
    let ts = e.ts.format("%H:%M:%S");
    let body = match &e.what {
        What::Session { event } => format!("session {event}"),
        What::Message { message, .. } => {
            let text = match message {
                Message::User(t) => format!("user: {t}"),
                Message::Assistant { text, tool_calls } => format!("assistant: {text} [{} calls]", tool_calls.len()),
                Message::ToolResult { content, .. } => format!("result: {content}"),
                Message::System(_) => "system".into(),
            };
            text.chars().take(160).collect()
        }
        What::Snapshot { messages, .. } => format!("snapshot ({} messages)", messages.len()),
        What::Goal { condition } => format!("goal {}", condition.as_deref().unwrap_or("cleared")),
        What::Output { tool, lines, .. } => format!("output {tool} ({lines} lines)"),
        What::File { path, via, diff, .. } => match diff {
            Some(d) => {
                let added: usize = d.iter().map(|h| h.2.lines().count()).sum();
                let removed: usize = d.iter().map(|h| h.1).sum();
                format!("file {} ({via}) +{added} -{removed}", path.display())
            }
            None => format!("file {} ({via})", path.display()),
        },
        What::Trace { level, target, message } => render_traces(level, target, message).chars().take(160).collect(),
    };
    format!("{ts} log#{id} a{} {body}", e.agent)
}

/// Harness diagnostics into the ledger: a tracing layer (replaces the old
/// `harness.log` file).
pub struct TraceLayer;

#[derive(Default)]
struct Fields {
    message: String,
    rest: Vec<String>,
}

impl tracing::field::Visit for Fields {
    fn record_debug(&mut self, field: &tracing::field::Field, value: &dyn std::fmt::Debug) {
        if field.name() == "message" {
            self.message = format!("{value:?}");
        } else {
            self.rest.push(format!("{}={value:?}", field.name()));
        }
    }
    fn record_str(&mut self, field: &tracing::field::Field, value: &str) {
        if field.name() == "message" {
            self.message = value.to_string();
        } else {
            self.rest.push(format!("{}={value}", field.name()));
        }
    }
}

impl<S: tracing::Subscriber> tracing_subscriber::Layer<S> for TraceLayer {
    fn on_event(&self, event: &tracing::Event<'_>, _ctx: tracing_subscriber::layer::Context<'_, S>) {
        let meta = event.metadata();
        let mut fields = Fields::default();
        event.record(&mut fields);
        let mut message = fields.message;
        for f in fields.rest {
            message.push(' ');
            message.push_str(&f);
        }
        let (level, target) = (meta.level().to_string(), meta.target().to_string());
        match PRIMARY.get() {
            Some(ledger) => {
                ledger.append(AgentId(0), What::Trace { level, target, message });
            }
            None => {
                let mut early = EARLY.lock().unwrap();
                if early.len() < 500 {
                    early.push((Utc::now(), level, target, message));
                }
            }
        }
    }
}

/// Read the whole ledger file (tests).
#[cfg(test)]
pub fn raw(project: &Path) -> String {
    std::fs::read_to_string(project.join(".tursi/ledger.jsonl")).unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("tursi-ledger-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn conversations_replay_from_the_latest_snapshot() {
        let dir = tmp("conv");
        let l = for_project(&dir);
        let s = Uuid::now_v7();
        l.set_session(s);
        l.append(AgentId(0), What::Message { task: 1, message: Message::User("first".into()) });
        l.append(AgentId(3), What::Message { task: 1, message: Message::User("child".into()) });
        l.append(AgentId(0), What::Snapshot { task: 1, goal: None, messages: vec![Message::User("compacted".into())] });
        l.append(AgentId(0), What::Message { task: 2, message: Message::User("after".into()) });
        l.append(AgentId(0), What::Goal { condition: Some("tests pass".into()) });
        let c = l.conversation(s, AgentId(0)).unwrap();
        assert_eq!(c.task, 2);
        assert_eq!(c.goal.as_deref(), Some("tests pass"));
        let texts: Vec<String> = c.messages.iter().map(|m| match m { Message::User(t) => t.clone(), _ => String::new() }).collect();
        assert_eq!(texts, vec!["compacted", "after"]);
        assert_eq!(l.conversation(s, AgentId(3)).unwrap().messages.len(), 1, "agents are separate");
        assert!(l.conversation(Uuid::now_v7(), AgentId(0)).is_none(), "sessions are separate");
    }

    #[test]
    fn outputs_are_addressable_searchable_and_big_ones_become_blobs() {
        let dir = tmp("out");
        let l = for_project(&dir);
        l.set_session(Uuid::now_v7());
        let small = l.output(AgentId(0), "execute_command", "ok\nerror[E0425]: cannot find value\nTOKEN=hunter2");
        let big_text: String = (0..3000).map(|i| format!("line {i} padding padding padding\n")).collect();
        let big = l.output(AgentId(2), "execute_command", &format!("{big_text}needle at the end"));
        assert!(raw(&dir).contains("\"blob\":"), "the big output went to a blob");
        assert!(!raw(&dir).contains("hunter2"), "redacted before it hit disk");
        let found = l.search("E0425", 1, 20);
        assert!(found.contains(&format!("log#{small}")) && found.contains("cannot find value"), "{found}");
        let found = l.search("needle", 0, 20);
        assert!(found.contains(&format!("log#{big} agent=2")), "{found}");
        assert!(l.search(&format!("log#{small}"), 0, 20).contains("cannot find value"));
        assert_eq!(l.search("absent", 0, 20), "no matches in the session log");
        let (hits, total) = l.grep("execute_command", 10);
        assert_eq!(total, 2);
        assert!(hits[0].contains("output execute_command"));
    }

    #[test]
    fn sessions_list_in_order_with_their_first_prompt() {
        let dir = tmp("sessions");
        let l = for_project(&dir);
        let (a, b) = (Uuid::now_v7(), Uuid::now_v7());
        l.set_session(a);
        l.append(AgentId(0), What::Session { event: "opened".into() });
        l.append(AgentId(0), What::Message { task: 1, message: Message::User("fix the bug".into()) });
        l.append(AgentId(0), What::Session { event: "closed".into() });
        l.set_session(b);
        l.append(AgentId(0), What::Session { event: "opened".into() });
        assert_eq!(l.session_ids(), vec![a, b]);
        let rows = l.sessions();
        assert_eq!(rows.len(), 2);
        assert!(rows[0].closed && rows[0].first_prompt == "fix the bug" && rows[0].messages == 1);
        assert!(!rows[1].closed);
    }

    #[test]
    fn blobs_are_written_once_by_hash() {
        let dir = tmp("blobs");
        let l = for_project(&dir);
        let h = l.put_blob(b"same").unwrap();
        assert_eq!(l.put_blob(b"same").unwrap(), h);
        assert_eq!(l.blob(&h).unwrap(), b"same");
    }
}
