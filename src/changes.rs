//! File changes, recorded in the project ledger (§3.3): every change to a
//! project file becomes a `file` event with its content hash before and
//! after and who made it.
//!
//! - `edit`/`write` record their own change, exactly (`record`).
//! - Shell commands are caught by comparing the project tree before and
//!   after each command (`begin`/`end`, or a `Watch` held while a background
//!   command runs), jj-style: stat fields first, and a file is re-read and
//!   hashed only when they moved. A change seen across a command is that
//!   agent's; if other agents' commands ran at the same time, they are listed
//!   in `overlap`. A change seen between commands, with nothing running, is
//!   `external` (an editor).
//!
//! Every version is identified by its blake3 hash and stored one of three
//! ways: as a diff from the version before it (in the event: an exact line
//! edit script), as a whole blob (a file's first version, every 32nd one, a
//! rewrite whose diff would be over half the file, binary content), or — for
//! a git-tracked file unchanged since the index — as git's own object, named
//! in the first event that changes it. `content` rebuilds any version and
//! checks it against its hash.
//!
//! Tracked: what `git ls-files` lists (tracked plus untracked, not ignored)
//! or, without git, a walk; either way skipping VCS, `.tursi` and the
//! directories that are only ever build output (`target/`, `node_modules/`,
//! …; not `out/` or `dist/`, which are often the result); files up to 1 MiB;
//! at most 50 000 files, past which shell capture switches off with a warning.

use anyhow::Result;
use std::collections::{BTreeSet, HashMap, HashSet};
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use crate::bus::AgentId;
use crate::diff::Hunk;
use crate::ledger::{Ledger, What};

const MAX_FILES: usize = 50_000;
const MAX_FILE_BYTES: u64 = 1 << 20;
/// A whole copy at least every this many versions, so no chain is longer.
const KEYFRAME_EVERY: u32 = 32;
/// Diffs up to this size ride on the event line; bigger ones are blobs.
const INLINE_DIFF: usize = 16 * 1024;
/// Never tracked, on top of the build-output directories.
const SKIP_DIRS: &[&str] = &[".git", ".hg", ".svn", ".jj", ".tursi"];

/// A file as last seen: the stat fields that move on any write, and the
/// content hash.
#[derive(Clone, PartialEq)]
struct Seen {
    stat: (i64, i64, i64, i64, u64, u64),
    hash: String,
}

/// Where a version's bytes live.
#[derive(Clone)]
enum Stored {
    /// `.tursi/blobs/<hash>`.
    Blob,
    /// A git object (`git cat-file blob <sha>`).
    Git(String),
    /// The `base` version with `script` applied.
    Delta { base: String, script: Arc<Vec<Hunk>> },
}

#[derive(Clone)]
struct Version {
    stored: Stored,
    /// Diffs since the last whole copy.
    depth: u32,
}

/// One change for the edit tool's oscillation probe.
struct Edit {
    agent: u32,
    task: u32,
    file: PathBuf,
    before: Option<String>,
}

/// What a look found for one path: before hash, after hash and bytes.
struct Found {
    path: PathBuf,
    before: Option<String>,
    after: Option<(String, Vec<u8>)>,
}

pub struct Changes {
    ledger: Arc<Ledger>,
    project: PathBuf,
    edits: Vec<Edit>,
    /// The tree as last seen; None until the first command.
    tree: Option<HashMap<PathBuf, Seen>>,
    /// Too many files: shell capture is off.
    off: bool,
    /// Commands running now: token → (agent, task, agents that ran alongside).
    running: HashMap<u64, (u32, u32, BTreeSet<u32>)>,
    next: u64,
    /// How this process stored each version it has seen.
    versions: HashMap<String, Version>,
    /// Versions stored by earlier sessions, read from the ledger on demand.
    recorded: Option<HashMap<String, Version>>,
    /// Who has read each file: a change by someone else becomes a note for
    /// them (the STALE fix: tell an agent when what it read moved).
    readers: HashMap<PathBuf, BTreeSet<u32>>,
    notes: HashMap<u32, Vec<(PathBuf, String)>>,
    /// Areas an agent declared it writes (a fork's subtask), while it runs.
    intents: HashMap<u32, Vec<PathBuf>>,
    /// Files each agent changed, by any means, since last asked (QA
    /// evidence: a shell-made change counts as much as an edit).
    changed: HashMap<u32, Vec<PathBuf>>,
}

fn hex(bytes: &[u8]) -> String {
    blake3::hash(bytes).to_hex().to_string()
}

impl Changes {
    pub fn open(project: &Path) -> Changes {
        Changes {
            ledger: crate::ledger::for_project(project),
            project: project.to_path_buf(),
            edits: Vec::new(),
            tree: None,
            off: false,
            running: HashMap::new(),
            next: 0,
            versions: HashMap::new(),
            recorded: None,
            readers: HashMap::new(),
            notes: HashMap::new(),
            intents: HashMap::new(),
            changed: HashMap::new(),
        }
    }

    /// Files `agent` changed since the last call, tools and shell alike.
    pub fn take_changed(&mut self, agent: AgentId) -> Vec<PathBuf> {
        self.changed.remove(&agent.0).unwrap_or_default()
    }

    /// `agent` read `file`: later changes by others are noted for it.
    pub fn note_read(&mut self, agent: AgentId, file: &Path) {
        self.readers.entry(file.to_path_buf()).or_default().insert(agent.0);
    }

    /// Changes others made to files this agent read, since it was last told
    /// (one line per file, newest wins).
    pub fn take_notes(&mut self, agent: AgentId) -> Vec<String> {
        self.notes.remove(&agent.0).unwrap_or_default().into_iter().map(|(_, line)| line).collect()
    }

    pub fn set_intent(&mut self, agent: AgentId, areas: Vec<PathBuf>) {
        self.intents.insert(agent.0, areas);
    }

    pub fn clear_intent(&mut self, agent: AgentId) {
        self.intents.remove(&agent.0);
    }

    /// Another agent declared it writes an area covering `file`.
    pub fn claimed_by_other(&self, agent: AgentId, file: &Path) -> Option<(u32, PathBuf)> {
        self.intents
            .iter()
            .filter(|(a, _)| **a != agent.0)
            .find_map(|(a, areas)| areas.iter().find(|area| file.starts_with(area)).map(|area| (*a, area.clone())))
    }

    fn notify(&mut self, author: u32, path: &Path, via: &str, delta: Option<(usize, usize)>) {
        let Some(readers) = self.readers.get(path) else { return };
        let who = match (via, author) {
            ("external", _) => "changed outside tursi".to_string(),
            (_, 0) => "the lead".to_string(),
            (_, n) => format!("agent-{n}"),
        };
        let delta = match delta {
            Some((added, removed)) => format!(", +{added} -{removed}"),
            None => String::new(),
        };
        let rel = path.strip_prefix(&self.project).unwrap_or(path).display().to_string();
        let line = format!("{rel} — {who} ({via}{delta})");
        for reader in readers.iter().filter(|r| **r != author || via == "external") {
            let pending = self.notes.entry(*reader).or_default();
            pending.retain(|(p, _)| p != path);
            pending.push((path.to_path_buf(), line.clone()));
        }
    }

    /// Record a change a tool just made: `before`/`after` are the content
    /// (None: absent). One event per edit-tool file transaction (§3.1).
    pub fn record(&mut self, agent: AgentId, task: u32, file: &Path, before: Option<&[u8]>, after: Option<&[u8]>, via: &str) -> Result<()> {
        let before_hash = before.map(hex);
        if let (Some(b), Some(h)) = (before, &before_hash) {
            if !self.versions.contains_key(h) {
                self.ledger.put_blob(b)?;
                self.versions.insert(h.clone(), Version { stored: Stored::Blob, depth: 0 });
            }
        }
        let after = after.map(|a| (hex(a), a));
        let (script, delta) = match &after {
            Some((h, a)) => self.store(before_hash.as_deref(), before, h, a)?,
            None => (None, None),
        };
        let after_hash = after.map(|(h, _)| h);
        self.append(agent.0, task, file.to_path_buf(), before_hash.clone(), after_hash.clone(), via, None, vec![], script, delta);
        // Keep the tree current, so the next shell look doesn't claim it.
        if let Some(tree) = &mut self.tree {
            match (after_hash, std::fs::metadata(file)) {
                (Some(hash), Ok(meta)) => {
                    tree.insert(file.to_path_buf(), Seen { stat: stat(&meta), hash });
                }
                _ => {
                    tree.remove(file);
                }
            }
        }
        self.edits.push(Edit { agent: agent.0, task, file: file.to_path_buf(), before: before_hash });
        Ok(())
    }

    /// Store a new version: a diff from `before` when that is small and the
    /// chain short, else a whole blob. Returns the diff when one was chosen.
    /// Also returns the change's size in lines (+added, -removed) when both
    /// sides are text, for change notes.
    fn store(&mut self, before_hash: Option<&str>, before: Option<&[u8]>, after_hash: &str, after: &[u8]) -> Result<(Option<Vec<Hunk>>, Option<(usize, usize)>)> {
        if let (Some(bh), Some(b)) = (before_hash, before) {
            if let (Ok(old), Ok(new)) = (std::str::from_utf8(b), std::str::from_utf8(after)) {
                let script = crate::diff::script(old, new);
                let delta = (script.iter().map(|h| h.2.lines().count()).sum(), script.iter().map(|h| h.1).sum());
                let depth = self.versions.get(bh).map_or(0, |v| v.depth);
                let size: usize = script.iter().map(|h| h.2.len() + 24).sum();
                // A version already stored (a file changed back) keeps its
                // storage: re-storing it as a diff against its own successor
                // would make a cycle that no rebuild can follow.
                if self.versions.contains_key(after_hash) {
                    return Ok((Some(script), Some(delta)));
                }
                if depth + 1 < KEYFRAME_EVERY && size * 2 <= after.len() {
                    let stored = Stored::Delta { base: bh.to_string(), script: Arc::new(script.clone()) };
                    self.versions.insert(after_hash.to_string(), Version { stored, depth: depth + 1 });
                    return Ok((Some(script), Some(delta)));
                }
                self.keyframe(after_hash, after)?;
                return Ok((None, Some(delta)));
            }
        }
        if !self.versions.contains_key(after_hash) {
            self.keyframe(after_hash, after)?;
        }
        Ok((None, None))
    }

    fn keyframe(&mut self, hash: &str, bytes: &[u8]) -> Result<()> {
        self.ledger.put_blob(bytes)?;
        self.versions.insert(hash.to_string(), Version { stored: Stored::Blob, depth: 0 });
        Ok(())
    }

    #[allow(clippy::too_many_arguments)]
    fn append(
        &mut self,
        agent: u32,
        task: u32,
        path: PathBuf,
        before: Option<String>,
        after: Option<String>,
        via: &str,
        command: Option<String>,
        overlap: Vec<u32>,
        script: Option<Vec<Hunk>>,
        delta: Option<(usize, usize)>,
    ) {
        let (diff, diff_blob) = match script {
            Some(s) => {
                let json = serde_json::to_vec(&s).unwrap_or_default();
                if json.len() <= INLINE_DIFF { (Some(s), None) } else { (None, self.ledger.put_blob(&json).ok()) }
            }
            None => (None, None),
        };
        let base_git = before.as_ref().and_then(|b| match self.versions.get(b).map(|v| &v.stored) {
            Some(Stored::Git(sha)) => Some(sha.clone()),
            _ => None,
        });
        self.notify(agent, &path, via, delta);
        if via != "external" {
            let mine = self.changed.entry(agent).or_default();
            if !mine.contains(&path) {
                mine.push(path.clone());
            }
        }
        self.ledger.append(
            AgentId(agent),
            What::File { task, path, before, after, via: via.to_string(), command, overlap, diff, diff_blob, base_git },
        );
    }

    /// Any version's bytes, rebuilt from where it is stored and checked
    /// against its hash.
    pub fn content(&mut self, hash: &str) -> Option<Vec<u8>> {
        self.content_from(hash, &mut HashSet::new())
    }

    /// `content`, refusing to follow a diff chain back into itself: an
    /// earlier session's ledger can hold a file that changed back (A → B →
    /// A) as two diffs against each other. A cycle, or any broken diff, falls
    /// back to the version's whole copy in the blob store.
    fn content_from(&mut self, hash: &str, seen: &mut HashSet<String>) -> Option<Vec<u8>> {
        if !seen.insert(hash.to_string()) {
            return None;
        }
        let version = match self.versions.get(hash) {
            Some(v) => v.clone(),
            // Not recorded as an `after` anywhere (a file's first version):
            // its whole copy is in the blob store.
            None => self.recorded().get(hash).cloned().unwrap_or(Version { stored: Stored::Blob, depth: 0 }),
        };
        let bytes = match version.stored {
            Stored::Blob => self.ledger.blob(hash).ok()?,
            Stored::Git(sha) => git_object(&self.project, &sha)?,
            Stored::Delta { base, script } => {
                let rebuilt = self
                    .content_from(&base, seen)
                    .and_then(|b| String::from_utf8(b).ok())
                    .and_then(|b| crate::diff::apply(&b, &script))
                    .map(String::into_bytes);
                match rebuilt {
                    Some(bytes) => bytes,
                    None => self.ledger.blob(hash).ok()?,
                }
            }
        };
        (hex(&bytes) == hash).then_some(bytes)
    }

    /// How earlier sessions stored their versions, from the ledger.
    fn recorded(&mut self) -> &HashMap<String, Version> {
        if self.recorded.is_none() {
            let mut map = HashMap::new();
            for (_, e) in self.ledger.events(&["\"kind\":\"file\""]) {
                let What::File { before, after, diff, diff_blob, base_git, .. } = e.what else { continue };
                if let (Some(b), Some(sha)) = (&before, base_git) {
                    map.entry(b.clone()).or_insert(Version { stored: Stored::Git(sha), depth: 0 });
                }
                let Some(a) = after else { continue };
                let script = diff.or_else(|| diff_blob.and_then(|h| self.ledger.blob(&h).ok()).and_then(|j| serde_json::from_slice(&j).ok()));
                match (script, before) {
                    (Some(script), Some(base)) => {
                        map.entry(a).or_insert(Version { stored: Stored::Delta { base, script: Arc::new(script) }, depth: 0 });
                    }
                    _ => {
                        if self.ledger.blob(&a).is_ok() {
                            map.entry(a).or_insert(Version { stored: Stored::Blob, depth: 0 });
                        }
                    }
                }
            }
            self.recorded = Some(map);
        }
        self.recorded.as_ref().unwrap()
    }

    /// Oscillation probe: was `file` already in exactly this state earlier in
    /// this agent's task? (§5.4)
    pub fn seen(&self, agent: AgentId, task: u32, file: &Path, hash: blake3::Hash) -> bool {
        let hex = hash.to_hex();
        self.edits
            .iter()
            .any(|e| e.agent == agent.0 && e.task == task && e.file == file && e.before.as_deref() == Some(hex.as_str()))
    }

    /// A command is about to run. Changes since the last look are recorded
    /// first: the running commands' if any are running, else `external`.
    pub fn begin(&mut self, agent: AgentId, task: u32) -> u64 {
        let token = self.next;
        self.next += 1;
        let pending = self.look();
        if !pending.is_empty() {
            match self.running.values().next().map(|(a, t, _)| (*a, *t)) {
                Some((owner, owner_task)) => {
                    let overlap: Vec<u32> = self.running.values().map(|(a, _, _)| *a).filter(|a| *a != owner).collect();
                    self.write(owner, owner_task, pending, "shell", None, overlap);
                }
                None => self.write(0, 0, pending, "external", None, vec![]),
            }
        }
        for (_, _, alongside) in self.running.values_mut() {
            alongside.insert(agent.0);
        }
        let alongside: BTreeSet<u32> = self.running.values().map(|(a, _, _)| *a).collect();
        self.running.insert(token, (agent.0, task, alongside));
        token
    }

    /// The command finished: what changed is attributed to it.
    pub fn end(&mut self, token: u64, command: &str) {
        let Some((agent, task, alongside)) = self.running.remove(&token) else { return };
        let found = self.look();
        if !found.is_empty() {
            let head: String = command.chars().take(200).collect();
            self.write(agent, task, found, "shell", Some(head), alongside.into_iter().filter(|a| *a != agent).collect());
        }
    }

    fn write(&mut self, agent: u32, task: u32, found: Vec<Found>, via: &str, command: Option<String>, overlap: Vec<u32>) {
        for f in found {
            let (script, delta) = match &f.after {
                Some((hash, bytes)) => {
                    let before = f.before.as_deref().and_then(|b| self.content(b));
                    self.store(f.before.as_deref(), before.as_deref(), hash, bytes).unwrap_or((None, None))
                }
                None => (None, None),
            };
            let after = f.after.map(|(h, _)| h);
            self.append(agent, task, f.path, f.before, after, via, command.clone(), overlap.clone(), script, delta);
        }
    }

    /// Rescan the tree; returns what changed since the last look. The first
    /// look only takes the baseline: each file's version is stored as git's
    /// object when git has it unchanged, else as a blob.
    fn look(&mut self) -> Vec<Found> {
        if self.off {
            return Vec::new();
        }
        let files = list(&self.project);
        if files.len() > MAX_FILES {
            self.off = true;
            tracing::warn!(files = files.len(), "more than {MAX_FILES} files: shell-made changes are not recorded for this project");
            return Vec::new();
        }
        let prev = self.tree.take();
        let in_git = if prev.is_none() { git_unchanged(&self.project) } else { HashMap::new() };
        let mut now = HashMap::with_capacity(files.len());
        let mut found = Vec::new();
        for path in files {
            let Ok(meta) = std::fs::symlink_metadata(&path) else { continue };
            if !meta.is_file() || meta.len() > MAX_FILE_BYTES {
                continue;
            }
            let s = stat(&meta);
            let old = prev.as_ref().and_then(|p| p.get(&path));
            if let Some(o) = old.filter(|o| o.stat == s) {
                now.insert(path, o.clone());
                continue;
            }
            let Ok(bytes) = std::fs::read(&path) else { continue };
            let hash = hex(&bytes);
            if prev.is_none() {
                if !self.versions.contains_key(&hash) {
                    let stored = match in_git.get(&path) {
                        Some(sha) => Stored::Git(sha.clone()),
                        None => {
                            if self.ledger.put_blob(&bytes).is_err() {
                                continue;
                            }
                            Stored::Blob
                        }
                    };
                    self.versions.insert(hash.clone(), Version { stored, depth: 0 });
                }
            } else if old.map(|o| &o.hash) != Some(&hash) {
                found.push(Found { path: path.clone(), before: old.map(|o| o.hash.clone()), after: Some((hash.clone(), bytes)) });
            }
            now.insert(path, Seen { stat: s, hash });
        }
        if let Some(prev) = &prev {
            for (path, old) in prev {
                if !now.contains_key(path) && !path.exists() {
                    found.push(Found { path: path.clone(), before: Some(old.hash.clone()), after: None });
                }
            }
        }
        self.tree = Some(now);
        found.sort_by(|a, b| a.path.cmp(&b.path));
        found
    }
}

/// Git-tracked files whose content git holds unchanged (index = worktree):
/// path → blob sha. Empty outside a repo.
fn git_unchanged(project: &Path) -> HashMap<PathBuf, String> {
    let git = |args: &[&str]| {
        std::process::Command::new("git").arg("-C").arg(project).args(args).output().ok().filter(|o| o.status.success()).map(|o| o.stdout)
    };
    if !project.join(".git").exists() {
        return HashMap::new();
    }
    let (Some(index), Some(modified)) = (git(&["ls-files", "-s", "-z"]), git(&["diff-files", "--name-only", "-z"])) else {
        return HashMap::new();
    };
    let modified: HashSet<&[u8]> = modified.split(|b| *b == 0).filter(|p| !p.is_empty()).collect();
    let mut out = HashMap::new();
    for entry in index.split(|b| *b == 0).filter(|e| !e.is_empty()) {
        // `<mode> <sha> <stage>\t<path>`
        let Some(tab) = entry.iter().position(|b| *b == b'\t') else { continue };
        let (meta, path) = (&entry[..tab], &entry[tab + 1..]);
        let fields: Vec<&[u8]> = meta.split(|b| *b == b' ').collect();
        if fields.len() != 3 || fields[2] != b"0" || modified.contains(path) {
            continue;
        }
        if let (Ok(sha), Ok(path)) = (std::str::from_utf8(fields[1]), std::str::from_utf8(path)) {
            out.insert(project.join(path), sha.to_string());
        }
    }
    out
}

fn git_object(project: &Path, sha: &str) -> Option<Vec<u8>> {
    let out = std::process::Command::new("git").arg("-C").arg(project).args(["cat-file", "blob", sha]).output().ok()?;
    out.status.success().then_some(out.stdout)
}

/// A running command's watch: ends (and attributes what changed) when
/// dropped, however the command's task ends — exit, timeout, or a `monitor
/// stop` that aborts it — so a stopped job never stays "running".
pub struct Watch {
    changes: Arc<Mutex<Changes>>,
    token: u64,
    command: String,
}

impl Drop for Watch {
    fn drop(&mut self) {
        if let Ok(mut changes) = self.changes.lock() {
            changes.end(self.token, &self.command);
        }
    }
}

/// Start watching for `agent`'s command; hold the guard until it finishes.
pub fn watch(changes: &Arc<Mutex<Changes>>, agent: AgentId, task: u32, command: &str) -> Watch {
    let token = changes.lock().unwrap().begin(agent, task);
    Watch { changes: changes.clone(), token, command: command.to_string() }
}

fn stat(meta: &std::fs::Metadata) -> (i64, i64, i64, i64, u64, u64) {
    (meta.mtime(), meta.mtime_nsec(), meta.ctime(), meta.ctime_nsec(), meta.len(), meta.ino())
}

fn skipped(name: &str) -> bool {
    SKIP_DIRS.contains(&name) || crate::sandbox::BUILD_DIRS.contains(&name)
}

/// Candidate files: git's view when the project is a repo, else a walk.
fn list(project: &Path) -> Vec<PathBuf> {
    if project.join(".git").exists() {
        let out = std::process::Command::new("git")
            .arg("-C")
            .arg(project)
            .args(["ls-files", "-z", "-c", "-o", "--exclude-standard"])
            .output();
        if let Ok(out) = out.map_err(|_| ()).and_then(|o| if o.status.success() { Ok(o) } else { Err(()) }) {
            return out
                .stdout
                .split(|b| *b == 0)
                .filter(|p| !p.is_empty())
                .filter_map(|p| std::str::from_utf8(p).ok())
                .filter(|p| !p.split('/').any(skipped))
                .map(|p| project.join(p))
                .collect();
        }
    }
    let mut out = Vec::new();
    let mut stack = vec![project.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&dir) else { continue };
        for entry in entries.flatten() {
            let Ok(kind) = entry.file_type() else { continue };
            let name = entry.file_name();
            if kind.is_dir() {
                if !skipped(&name.to_string_lossy()) {
                    stack.push(entry.path());
                }
            } else if kind.is_file() {
                out.push(entry.path());
                if out.len() > MAX_FILES {
                    return out;
                }
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn events(dir: &Path) -> Vec<serde_json::Value> {
        crate::ledger::raw(dir).lines().filter_map(|l| serde_json::from_str(l).ok()).filter(|e: &serde_json::Value| e["kind"] == "file").collect()
    }

    #[test]
    fn tool_edits_are_recorded_and_probed_per_agent() {
        let dir = crate::tools::testutil::tmp("changes-edit");
        let mut c = Changes::open(&dir);
        let file = dir.join("a.txt");
        std::fs::write(&file, "two").unwrap();
        c.record(AgentId(3), 1, &file, Some(b"one"), Some(b"two"), "edit").unwrap();
        assert!(c.seen(AgentId(3), 1, &file, blake3::hash(b"one")));
        assert!(!c.seen(AgentId(0), 1, &file, blake3::hash(b"one")), "per agent");
        let e = &events(&dir)[0];
        assert!(e["agent"] == 3 && e["via"] == "edit" && e["before"].is_string() && e["after"].is_string());
    }

    #[test]
    fn shell_changes_are_attributed_to_the_command_that_made_them() {
        let dir = crate::tools::testutil::tmp("changes-shell");
        std::fs::write(dir.join("keep.rs"), "fn a() {}").unwrap();
        std::fs::write(dir.join("gone.rs"), "x").unwrap();
        std::fs::create_dir_all(dir.join("target")).unwrap();
        let mut c = Changes::open(&dir);

        let t = c.begin(AgentId(2), 1); // baseline
        std::fs::write(dir.join("keep.rs"), "fn a() { 1 }").unwrap();
        std::fs::write(dir.join("new.rs"), "fn b() {}").unwrap();
        std::fs::remove_file(dir.join("gone.rs")).unwrap();
        std::fs::write(dir.join("target/out.o"), "build output").unwrap();
        // `out/` is often the deliverable (a benchmark's results): recorded.
        std::fs::create_dir_all(dir.join("out")).unwrap();
        std::fs::write(dir.join("out/result.json"), "{}").unwrap();
        c.end(t, "sed -i s/x/y/ keep.rs && rm gone.rs");
        let e = events(&dir);
        let paths: Vec<String> = e.iter().map(|e| e["path"].as_str().unwrap().rsplit('/').next().unwrap().to_string()).collect();
        assert_eq!(paths, vec!["gone.rs", "keep.rs", "new.rs", "result.json"], "build output untracked");
        assert!(e.iter().all(|e| e["agent"] == 2 && e["via"] == "shell" && e["command"].as_str().unwrap().starts_with("sed")));
        assert!(e[0]["after"].is_null() && e[2]["before"].is_null());

        // Between commands, with nothing running: external.
        std::fs::write(dir.join("keep.rs"), "fn a() { 2 }").unwrap();
        let t = c.begin(AgentId(4), 2);
        c.end(t, "true");
        let last = events(&dir).pop().unwrap();
        assert!(last["via"] == "external" && last["path"].as_str().unwrap().ends_with("keep.rs"));
    }

    fn blobs(dir: &Path) -> usize {
        fn walk(d: &Path) -> usize {
            std::fs::read_dir(d).map(|it| it.flatten().map(|e| if e.path().is_dir() { walk(&e.path()) } else { 1 }).sum()).unwrap_or(0)
        }
        walk(&dir.join(".tursi/blobs"))
    }

    #[test]
    fn edits_are_stored_as_diffs_with_periodic_whole_copies_and_rebuild_exactly() {
        let dir = crate::tools::testutil::tmp("changes-chain");
        let file = dir.join("big.rs");
        let mut text: String = (0..200).map(|i| format!("fn f{i}() {{ {i} }}\n")).collect();
        let mut versions = vec![text.clone()];
        let mut c = Changes::open(&dir);
        for k in 0..40 {
            let before = text.clone();
            text = text.replacen(&format!("{{ {k} }}"), &format!("{{ {k} + 1 }}"), 1);
            std::fs::write(&file, &text).unwrap();
            c.record(AgentId(1), 1, &file, Some(before.as_bytes()), Some(text.as_bytes()), "edit").unwrap();
            versions.push(text.clone());
        }
        let e = events(&dir);
        let diffs = e.iter().filter(|e| e["diff"].is_array()).count();
        assert_eq!(diffs, 39, "one whole copy at the 32nd version, the rest diffs");
        assert_eq!(blobs(&dir), 2, "the first version and one keyframe — not 41 copies");
        // Every version rebuilds, in this process and from the ledger alone.
        let mut fresh = Changes::open(&dir);
        for v in &versions {
            let h = hex(v.as_bytes());
            assert_eq!(c.content(&h).as_deref(), Some(v.as_bytes()));
            assert_eq!(fresh.content(&h).as_deref(), Some(v.as_bytes()), "rebuilt from the ledger");
        }
        // A rewrite whose diff would be most of the file is a whole copy.
        let before = text.clone();
        let rewrite: String = (0..200).map(|i| format!("pub fn g{i}() -> u32 {{ {i} }}\n")).collect();
        std::fs::write(&file, &rewrite).unwrap();
        c.record(AgentId(1), 1, &file, Some(before.as_bytes()), Some(rewrite.as_bytes()), "write").unwrap();
        assert!(events(&dir).last().unwrap().get("diff").is_none());
        // Binary content is always whole.
        let bin = dir.join("x.bin");
        std::fs::write(&bin, [0u8, 159, 146, 150]).unwrap();
        c.record(AgentId(1), 1, &bin, Some(&[0u8, 159, 146, 1]), Some(&[0u8, 159, 146, 150]), "write").unwrap();
        assert!(events(&dir).last().unwrap().get("diff").is_none());
    }

    #[test]
    fn a_file_changed_back_rebuilds_without_a_cycle() {
        // The crash on Harness-Bench 062: a CSV rewritten A → B → A stored A
        // as a diff against B and B against A, and rebuilding recursed forever.
        let dir = crate::tools::testutil::tmp("changes-flip");
        let file = dir.join("out.csv");
        let a: String = (0..60).map(|i| format!("row,{i},ok\n")).collect();
        let b = a.replacen("row,7,ok", "row,7,FAIL", 1);
        let mut c = Changes::open(&dir);
        for (before, after) in [(&a, &b), (&b, &a), (&a, &b), (&b, &a)] {
            std::fs::write(&file, after).unwrap();
            c.record(AgentId(1), 1, &file, Some(before.as_bytes()), Some(after.as_bytes()), "write").unwrap();
        }
        let (ha, hb) = (hex(a.as_bytes()), hex(b.as_bytes()));
        assert_eq!(c.content(&ha).as_deref(), Some(a.as_bytes()));
        assert_eq!(c.content(&hb).as_deref(), Some(b.as_bytes()));
        // A ledger written that way (the first version never an `after`, so
        // it is re-recorded as a diff against its successor) still rebuilds.
        let mut fresh = Changes::open(&dir);
        assert_eq!(fresh.content(&ha).as_deref(), Some(a.as_bytes()), "from the blob store");
        assert_eq!(fresh.content(&hb).as_deref(), Some(b.as_bytes()));
    }

    #[test]
    fn a_git_baseline_copies_nothing_and_rebuilds_from_git() {
        let dir = crate::tools::testutil::tmp("changes-git");
        let git = |args: &[&str]| assert!(std::process::Command::new("git").arg("-C").arg(&dir).args(args).output().unwrap().status.success());
        git(&["init", "-q"]);
        let original: String = (0..100).map(|i| format!("line {i}\n")).collect();
        std::fs::write(dir.join("tracked.txt"), &original).unwrap();
        git(&["add", "tracked.txt"]);
        git(&["-c", "user.email=t@t", "-c", "user.name=t", "commit", "-qm", "x"]);
        let mut c = Changes::open(&dir);
        let t = c.begin(AgentId(2), 1);
        assert_eq!(blobs(&dir), 0, "git already has the baseline");
        let changed = original.replace("line 50\n", "line fifty\n");
        std::fs::write(dir.join("tracked.txt"), &changed).unwrap();
        c.end(t, "sed -i s/50/fifty/ tracked.txt");
        let e = events(&dir).pop().unwrap();
        assert!(e["base_git"].is_string() && e["diff"].is_array(), "{e}");
        assert_eq!(blobs(&dir), 0, "a small change is a diff on git's copy");
        let mut fresh = Changes::open(&dir);
        assert_eq!(fresh.content(e["before"].as_str().unwrap()).as_deref(), Some(original.as_bytes()));
        assert_eq!(fresh.content(e["after"].as_str().unwrap()).as_deref(), Some(changed.as_bytes()));
    }

    #[test]
    fn readers_hear_about_other_agents_changes_and_areas_are_claimed() {
        let dir = crate::tools::testutil::tmp("changes-notes");
        let file = dir.join("api.rs");
        std::fs::write(&file, "fn a() {}\nfn b() {}\nfn c() {}\nfn d() {}\n").unwrap();
        let mut c = Changes::open(&dir);
        c.note_read(AgentId(1), &file);
        c.note_read(AgentId(2), &file);
        std::fs::write(&file, "fn a() {}\nfn b() { 2 }\nfn c() {}\nfn d() {}\n").unwrap();
        c.record(AgentId(2), 1, &file, Some(b"fn a() {}\nfn b() {}\nfn c() {}\nfn d() {}\n"), Some(b"fn a() {}\nfn b() { 2 }\nfn c() {}\nfn d() {}\n"), "edit").unwrap();
        assert_eq!(c.take_notes(AgentId(1)), vec!["api.rs — agent-2 (edit, +1 -1)"]);
        assert!(c.take_notes(AgentId(1)).is_empty(), "told once");
        assert!(c.take_notes(AgentId(2)).is_empty(), "not about its own change");
        c.set_intent(AgentId(4), vec![dir.join("src/net")]);
        assert_eq!(c.claimed_by_other(AgentId(5), &dir.join("src/net/http.rs")).map(|(a, _)| a), Some(4));
        assert!(c.claimed_by_other(AgentId(4), &dir.join("src/net/http.rs")).is_none(), "its own area");
        assert!(c.claimed_by_other(AgentId(5), &dir.join("src/ui.rs")).is_none());
        c.clear_intent(AgentId(4));
        assert!(c.claimed_by_other(AgentId(5), &dir.join("src/net/http.rs")).is_none());
    }

    #[tokio::test]
    async fn a_background_watch_credits_its_agent_when_the_task_ends_or_is_aborted() {
        let dir = crate::tools::testutil::tmp("changes-bg");
        std::fs::write(dir.join("a.rs"), "a").unwrap();
        let changes = Arc::new(Mutex::new(Changes::open(&dir)));
        // Baseline first, as the session's earlier commands would have.
        drop(watch(&changes, AgentId(0), 0, "true"));
        let w = watch(&changes, AgentId(5), 2, "sh gen.sh");
        let job = tokio::spawn(async move {
            let _w = w;
            tokio::time::sleep(std::time::Duration::from_secs(30)).await;
        });
        std::fs::write(dir.join("a.rs"), "b").unwrap();
        job.abort(); // `monitor stop`
        let _ = job.await;
        let e = events(&dir);
        assert_eq!(e.len(), 1, "{e:?}");
        assert!(e[0]["agent"] == 5 && e[0]["task"] == 2 && e[0]["command"] == "sh gen.sh");
        assert!(changes.lock().unwrap().running.is_empty(), "the aborted job is not still running");
    }

    #[test]
    fn overlapping_commands_are_noted_and_tool_edits_are_not_double_counted() {
        let dir = crate::tools::testutil::tmp("changes-overlap");
        std::fs::write(dir.join("a.rs"), "a").unwrap();
        let mut c = Changes::open(&dir);
        let t1 = c.begin(AgentId(1), 1);
        let t2 = c.begin(AgentId(2), 1);
        std::fs::write(dir.join("a.rs"), "b").unwrap();
        c.end(t1, "make");
        c.end(t2, "make");
        let e = events(&dir);
        assert_eq!(e.len(), 1);
        assert!(e[0]["agent"] == 1 && e[0]["overlap"] == serde_json::json!([2]));
        // A tool edit updates the tree: the next look doesn't claim it.
        std::fs::write(dir.join("a.rs"), "c").unwrap();
        c.record(AgentId(1), 1, &dir.join("a.rs"), Some(b"b"), Some(b"c"), "edit").unwrap();
        let t = c.begin(AgentId(1), 1);
        c.end(t, "true");
        assert_eq!(events(&dir).len(), 2, "the edit alone");
    }
}
