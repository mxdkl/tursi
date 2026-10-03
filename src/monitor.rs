//! Monitors: wait without spending. The model arms a watch — files under the
//! project, or a long-running command — ends its turn, and is woken with
//! what happened. Monitors are session-scoped: they outlive the task that
//! armed them; an event while idle starts a new turn, an event mid-task is
//! injected at the next iteration like steering (§5.2).

use anyhow::{Result, anyhow, bail};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, SystemTime};
use tokio::io::AsyncBufReadExt;
use tokio::sync::mpsc;

use crate::sandbox::{Sandbox, Spawn, Stream};

/// Armed monitors per session.
const MAX_MONITORS: usize = 4;
/// Filesystem poll period (no inotify dependency inside the sandbox).
const POLL: Duration = Duration::from_millis(500);
/// Command output is batched this long before a wake.
const DEBOUNCE: Duration = Duration::from_millis(1000);
/// Lines per wake; the rest is summarized by count.
const MAX_LINES: usize = 50;

#[derive(Debug, Clone)]
pub struct Event {
    pub id: String,
    pub label: String,
    pub what: What,
}

#[derive(Debug, Clone)]
pub enum What {
    /// Files created / modified / deleted since the last wake.
    Files { created: Vec<String>, modified: Vec<String>, deleted: Vec<String> },
    /// New output lines from a command monitor (plus how many were dropped).
    Output { lines: Vec<String>, dropped: usize },
    /// The command exited; the monitor is gone. `tail`: the error-aware
    /// extract of its output (background calls), empty for streamed ones.
    Exited { code: Option<i32>, tail: String },
    /// Timed out; the monitor is gone.
    TimedOut { after: Duration },
}

impl Event {
    /// The user-role injection the model sees.
    pub fn render(&self) -> String {
        let head = format!("[monitor {} {}]", self.id, self.label);
        match &self.what {
            What::Files { created, modified, deleted } => {
                let mut parts = Vec::new();
                for (verb, list) in [("created", created), ("modified", modified), ("deleted", deleted)] {
                    if !list.is_empty() {
                        parts.push(format!("{verb} {}", list.join(", ")));
                    }
                }
                format!("{head} {}", parts.join("; "))
            }
            What::Output { lines, dropped } => {
                let mut s = format!("{head} {} new line(s):\n{}", lines.len() + dropped, lines.join("\n"));
                if *dropped > 0 {
                    s.push_str(&format!("\n… {dropped} more (log_search has them)"));
                }
                s
            }
            What::Exited { code, tail } => {
                let status = match code {
                    Some(0) => "finished: exit 0".to_string(),
                    Some(c) => format!("FAILED: exit {c}"),
                    None => "killed (timeout or signal)".to_string(),
                };
                if tail.trim().is_empty() {
                    format!("{head} {status} — monitor removed")
                } else {
                    format!("{head} {status} — monitor removed\n{tail}")
                }
            }
            What::TimedOut { after } => format!("{head} timed out after {}s — monitor removed", after.as_secs()),
        }
    }
}

struct Armed {
    label: String,
    kind: String,
    task: tokio::task::JoinHandle<()>,
}

/// Owned by the toolbox; the loop holds the receiver.
pub struct Manager {
    armed: HashMap<String, Armed>,
    next: u32,
    events: mpsc::Sender<Event>,
    sandbox: Sandbox,
}

impl Manager {
    pub fn new(sandbox: Sandbox) -> (Manager, mpsc::Receiver<Event>) {
        let (tx, rx) = mpsc::channel(64);
        (Manager { armed: HashMap::new(), next: 1, events: tx, sandbox }, rx)
    }

    /// `(id, label, kind)` of each armed monitor.
    pub fn list(&self) -> Vec<(String, String, String)> {
        let mut v: Vec<_> = self.armed.iter().map(|(id, a)| (id.clone(), a.label.clone(), a.kind.clone())).collect();
        v.sort();
        v
    }

    pub fn stop(&mut self, id: &str) -> bool {
        match self.armed.remove(id) {
            Some(a) => {
                a.task.abort();
                true
            }
            None => false,
        }
    }

    /// Drop a monitor that ended on its own (exit/timeout) from the list.
    pub fn forget(&mut self, id: &str) {
        self.armed.remove(id);
    }

    fn reserve(&mut self, label: &str, kind: &str) -> Result<String> {
        if self.armed.len() >= MAX_MONITORS {
            bail!("{MAX_MONITORS} monitors are already armed — stop one first ({})", self.list().iter().map(|(i, l, _)| format!("{i} {l}")).collect::<Vec<_>>().join(", "));
        }
        let id = format!("m{}", self.next);
        self.next += 1;
        let _ = (label, kind);
        Ok(id)
    }

    /// Watch files under `paths` (project-relative files or directories,
    /// recursive) whose names match `pattern` (`*` wildcard; default all).
    pub fn watch_paths(&mut self, paths: Vec<PathBuf>, pattern: Option<String>, label: String, timeout: Option<Duration>) -> Result<String> {
        if paths.is_empty() {
            bail!("give at least one path");
        }
        let roots: Vec<PathBuf> = paths.iter().map(|p| self.sandbox.resolve(p, false)).collect::<Result<_>>()?;
        let id = self.reserve(&label, "paths")?;
        let (events, id2, label2, project) = (self.events.clone(), id.clone(), label.clone(), self.sandbox.project.clone());
        let task = tokio::spawn(async move {
            let started = Instant::now();
            let mut seen = snapshot(&roots, pattern.as_deref());
            loop {
                tokio::time::sleep(POLL).await;
                if timeout.is_some_and(|t| started.elapsed() >= t) {
                    let _ = events.send(Event { id: id2.clone(), label: label2.clone(), what: What::TimedOut { after: started.elapsed() } }).await;
                    return;
                }
                let now = snapshot(&roots, pattern.as_deref());
                let rel = |p: &PathBuf| p.strip_prefix(&project).unwrap_or(p).display().to_string();
                let created: Vec<String> = now.keys().filter(|p| !seen.contains_key(*p)).map(rel).collect();
                let deleted: Vec<String> = seen.keys().filter(|p| !now.contains_key(*p)).map(rel).collect();
                let modified: Vec<String> = now.iter().filter(|(p, m)| seen.get(*p).is_some_and(|old| old != *m)).map(|(p, _)| rel(p)).collect();
                if !(created.is_empty() && deleted.is_empty() && modified.is_empty()) {
                    let _ = events.send(Event { id: id2.clone(), label: label2.clone(), what: What::Files { created, modified, deleted } }).await;
                }
                seen = now;
            }
        });
        self.armed.insert(id.clone(), Armed { label, kind: "paths".into(), task });
        Ok(id)
    }

    /// Run `command` in the sandbox and wake on its output lines (batched)
    /// and its exit. With `quiet`, only the exit wakes — with an error-aware
    /// tail of everything it printed (background builds and test runs).
    pub async fn watch_command(&mut self, command: String, label: String, timeout: Option<Duration>, quiet: bool) -> Result<String> {
        crate::shell::validate_step(&command, self.sandbox.bash)?;
        let id = self.reserve(&label, if quiet { "background" } else { "command" })?;
        let argv = vec![self.sandbox.shell.clone(), "-c".to_string(), command.clone()];
        let mut child = self
            .sandbox
            .spawn(Spawn { argv, env: vec![], cwd: None, stdin: Stream::Null, stdout: Stream::Piped, stderr: Stream::Piped })
            .await?;
        let stdout = child.stdout.take().ok_or_else(|| anyhow!("no stdout"))?;
        let stderr = child.stderr.take().ok_or_else(|| anyhow!("no stderr"))?;
        let (events, id2, label2, project, agent) = (self.events.clone(), id.clone(), label.clone(), self.sandbox.project.clone(), crate::bus::ROOT);
        let task = tokio::spawn(async move {
            let started = Instant::now();
            let (line_tx, mut line_rx) = mpsc::channel::<String>(256);
            for stream in [stdout, stderr] {
                let tx = line_tx.clone();
                tokio::spawn(async move {
                    let mut lines = tokio::io::BufReader::new(stream).lines();
                    while let Ok(Some(line)) = lines.next_line().await {
                        if tx.send(line).await.is_err() {
                            break;
                        }
                    }
                });
            }
            drop(line_tx);
            let mut pending: Vec<String> = Vec::new();
            let mut full = String::new();
            let mut deadline: Option<Instant> = None;
            loop {
                let wait = match deadline {
                    Some(d) => d.saturating_duration_since(Instant::now()),
                    None => Duration::from_secs(3600),
                };
                let timed_out = timeout.is_some_and(|t| started.elapsed() >= t);
                if timed_out {
                    child.kill();
                    let _ = events.send(Event { id: id2.clone(), label: label2.clone(), what: What::TimedOut { after: started.elapsed() } }).await;
                    return;
                }
                tokio::select! {
                    line = line_rx.recv() => match line {
                        Some(line) => {
                            full.push_str(&line);
                            full.push('\n');
                            if !quiet {
                                pending.push(line);
                                deadline.get_or_insert(Instant::now() + DEBOUNCE);
                            }
                        }
                        None => {
                            // Streams closed: flush, then report the exit.
                            if !pending.is_empty() {
                                let _ = events.send(Event { id: id2.clone(), label: label2.clone(), what: batch(&mut pending) }).await;
                            }
                            let code = child.wait().await;
                            let mut tail = String::new();
                            if !full.trim().is_empty() {
                                let log = crate::output::log_full(&project, agent, "monitor", &full);
                                if quiet {
                                    tail = crate::output::truncate(&full, 20);
                                    if let Ok(id) = log {
                                        tail.push_str(&format!("\n[full: log#{}]", id.0));
                                    }
                                }
                            }
                            let _ = events.send(Event { id: id2.clone(), label: label2.clone(), what: What::Exited { code, tail } }).await;
                            return;
                        }
                    },
                    _ = tokio::time::sleep(wait.min(POLL)) => {
                        if deadline.is_some_and(|d| Instant::now() >= d) {
                            deadline = None;
                            let _ = events.send(Event { id: id2.clone(), label: label2.clone(), what: batch(&mut pending) }).await;
                        }
                    }
                }
            }
        });
        self.armed.insert(id.clone(), Armed { label, kind: if quiet { "background".into() } else { "command".into() }, task });
        Ok(id)
    }
}

fn batch(pending: &mut Vec<String>) -> What {
    let dropped = pending.len().saturating_sub(MAX_LINES);
    let lines: Vec<String> = pending.drain(..).skip(dropped).collect();
    What::Output { lines, dropped }
}

/// path → mtime for every matching file under the roots.
fn snapshot(roots: &[PathBuf], pattern: Option<&str>) -> HashMap<PathBuf, SystemTime> {
    let mut out = HashMap::new();
    for root in roots {
        walk(root, pattern, &mut out, 0);
    }
    out
}

fn walk(path: &Path, pattern: Option<&str>, out: &mut HashMap<PathBuf, SystemTime>, depth: usize) {
    let Ok(meta) = std::fs::symlink_metadata(path) else { return };
    if meta.is_dir() {
        if depth > 12 || path.file_name().is_some_and(|n| matches!(n.to_str(), Some(".git" | "node_modules" | "target" | ".tursi"))) {
            return;
        }
        if let Ok(entries) = std::fs::read_dir(path) {
            for e in entries.flatten() {
                walk(&e.path(), pattern, out, depth + 1);
            }
        }
    } else if meta.is_file() {
        let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("");
        if pattern.is_none_or(|p| wildcard(p, name)) {
            out.insert(path.to_path_buf(), meta.modified().unwrap_or(SystemTime::UNIX_EPOCH));
        }
    }
}

/// `*`-only glob on a file name.
fn wildcard(pattern: &str, text: &str) -> bool {
    let (p, t): (Vec<char>, Vec<char>) = (pattern.chars().collect(), text.chars().collect());
    let (mut pi, mut ti, mut star, mut mark) = (0, 0, None, 0);
    while ti < t.len() {
        if pi < p.len() && p[pi] == t[ti] {
            pi += 1;
            ti += 1;
        } else if pi < p.len() && p[pi] == '*' {
            star = Some(pi);
            mark = ti;
            pi += 1;
        } else if let Some(s) = star {
            pi = s + 1;
            mark += 1;
            ti = mark;
        } else {
            return false;
        }
    }
    while pi < p.len() && p[pi] == '*' {
        pi += 1;
    }
    pi == p.len()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tools::testutil;

    #[tokio::test]
    async fn a_paths_monitor_wakes_on_create_modify_delete() {
        let dir = testutil::tmp("mon-paths");
        std::fs::create_dir_all(dir.join("inbox")).unwrap();
        let (mut m, mut rx) = Manager::new(Sandbox::for_tests(&dir));
        let id = m.watch_paths(vec!["inbox".into()], Some("*.md".into()), "inbox".into(), None).unwrap();
        assert_eq!(id, "m1");
        tokio::time::sleep(Duration::from_millis(100)).await;
        std::fs::write(dir.join("inbox/hello.md"), "hi").unwrap();
        std::fs::write(dir.join("inbox/ignored.txt"), "no").unwrap();
        let ev = tokio::time::timeout(Duration::from_secs(3), rx.recv()).await.unwrap().unwrap();
        assert!(matches!(&ev.what, What::Files { created, .. } if created == &vec!["inbox/hello.md".to_string()]), "{ev:?}");
        assert_eq!(ev.render(), "[monitor m1 inbox] created inbox/hello.md");
        // Modify (a second later so mtime differs), then delete.
        tokio::time::sleep(Duration::from_millis(1100)).await;
        std::fs::write(dir.join("inbox/hello.md"), "changed").unwrap();
        let ev = tokio::time::timeout(Duration::from_secs(3), rx.recv()).await.unwrap().unwrap();
        assert!(matches!(&ev.what, What::Files { modified, .. } if modified == &vec!["inbox/hello.md".to_string()]), "{ev:?}");
        std::fs::remove_file(dir.join("inbox/hello.md")).unwrap();
        let ev = tokio::time::timeout(Duration::from_secs(3), rx.recv()).await.unwrap().unwrap();
        assert!(matches!(&ev.what, What::Files { deleted, .. } if deleted.len() == 1));
        assert!(m.stop("m1") && !m.stop("m1"));
        assert!(m.list().is_empty());
    }

    #[tokio::test]
    async fn a_command_monitor_batches_lines_and_reports_exit() {
        let dir = testutil::tmp("mon-cmd");
        let (mut m, mut rx) = Manager::new(Sandbox::for_tests(&dir));
        let id = m.watch_command("echo one; echo two; sleep 0.2; echo three".into(), "build".into(), None, false).await;
        // `;` is rejected like any step: use a script file instead.
        assert!(id.is_err(), "chaining is rejected");
        std::fs::write(dir.join("run.sh"), "echo one\necho two\nsleep 0.3\necho three\nexit 3\n").unwrap();
        let id = m.watch_command("sh run.sh".into(), "build".into(), None, false).await.unwrap();
        let first = tokio::time::timeout(Duration::from_secs(5), rx.recv()).await.unwrap().unwrap();
        assert!(matches!(&first.what, What::Output { lines, .. } if lines.len() >= 2 && lines[0] == "one"), "{first:?}");
        let mut saw_exit = false;
        for _ in 0..3 {
            let ev = tokio::time::timeout(Duration::from_secs(5), rx.recv()).await.unwrap().unwrap();
            if let What::Exited { code, .. } = ev.what {
                assert_eq!(code, Some(3));
                saw_exit = true;
                break;
            }
        }
        assert!(saw_exit);
        m.forget(&id);
        assert!(m.list().is_empty());
    }

    #[tokio::test]
    async fn a_quiet_command_wakes_only_on_exit_with_a_tail() {
        let dir = testutil::tmp("mon-quiet");
        let (mut m, mut rx) = Manager::new(Sandbox::for_tests(&dir));
        std::fs::write(dir.join("build.sh"), "echo compiling\necho error[E0308]: mismatched types\nexit 101\n").unwrap();
        let id = m.watch_command("sh build.sh".into(), "cargo build".into(), None, true).await.unwrap();
        let ev = tokio::time::timeout(Duration::from_secs(5), rx.recv()).await.unwrap().unwrap();
        let text = ev.render();
        assert!(text.starts_with(&format!("[monitor {id} cargo build] FAILED: exit 101")), "{text}");
        assert!(text.contains("error[E0308]") && text.contains("[full: log#"), "{text}");
        assert!(rx.try_recv().is_err(), "no per-line wakes in quiet mode");
    }

    #[test]
    fn wildcard_matches_names() {
        assert!(wildcard("*.md", "a.md") && !wildcard("*.md", "a.txt") && wildcard("reply-*", "reply-3") && wildcard("*", "x"));
    }
}
