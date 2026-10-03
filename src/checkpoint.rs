//! Pre-mutation snapshots and `:rewind` (§3.3). Also the oscillation probe
//! behind the edit tool's "undo loop?" note.

use anyhow::{Context, Result};
use std::path::{Path, PathBuf};

pub struct Store {
    dir: PathBuf,
    entries: Vec<Checkpoint>,
}

pub struct Checkpoint {
    pub seq: u64,
    pub task: u32,
    pub file: PathBuf,
    pub hash: blake3::Hash,
    /// A snapshot of a not-yet-existing file: rewind deletes it.
    pub existed: bool,
}

impl Store {
    /// Session checkpoint directories older than this are swept at open.
    const MAX_AGE: std::time::Duration = std::time::Duration::from_secs(7 * 24 * 3600);

    /// `.tursi/checkpoints/<session>/`, with an `index.jsonl` beside the
    /// snapshots so a resumed session can still `:rewind` (§9). Other
    /// sessions' directories older than `MAX_AGE` are removed here.
    pub fn open(project: &Path, session: uuid::Uuid) -> Result<Store> {
        let root = project.join(".tursi/checkpoints");
        let dir = root.join(session.to_string());
        std::fs::create_dir_all(&dir)?;
        for entry in std::fs::read_dir(&root)?.flatten() {
            let old = entry.path() != dir
                && entry.metadata().and_then(|m| m.modified()).ok().and_then(|t| t.elapsed().ok()).is_some_and(|age| age > Self::MAX_AGE);
            if old {
                let _ = std::fs::remove_dir_all(entry.path());
            }
        }
        let mut entries = Vec::new();
        for line in std::fs::read_to_string(dir.join("index.jsonl")).unwrap_or_default().lines() {
            if let Ok(v) = serde_json::from_str::<serde_json::Value>(line)
                && let (Some(seq), Some(task), Some(file), Some(hash), Some(existed)) = (
                    v.get("seq").and_then(|x| x.as_u64()),
                    v.get("task").and_then(|x| x.as_u64()),
                    v.get("file").and_then(|x| x.as_str()),
                    v.get("hash").and_then(|x| x.as_str()).and_then(|h| blake3::Hash::from_hex(h).ok()),
                    v.get("existed").and_then(|x| x.as_bool()),
                )
            {
                entries.push(Checkpoint { seq, task: task as u32, file: PathBuf::from(file), hash, existed });
            }
        }
        Ok(Store { dir, entries })
    }

    /// Snapshot before any mutation of `file` — one snapshot per edit-tool
    /// file transaction (§3.1), not per hunk. Missing files snapshot as
    /// "absent" so rewinding a fresh `write` deletes the file again.
    pub fn snapshot(&mut self, task: u32, file: &Path) -> Result<()> {
        let seq = self.entries.len() as u64;
        let (content, existed) = match std::fs::read(file) {
            Ok(bytes) => (bytes, true),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => (Vec::new(), false),
            Err(e) => return Err(e).with_context(|| format!("snapshotting {}", file.display())),
        };
        std::fs::write(self.dir.join(format!("{seq:05}.snap")), &content)?;
        let hash = blake3::hash(&content);
        {
            use std::io::Write;
            let mut index = std::fs::OpenOptions::new().create(true).append(true).open(self.dir.join("index.jsonl"))?;
            writeln!(index, "{}", serde_json::json!({"seq": seq, "task": task, "file": file, "hash": hash.to_hex().as_str(), "existed": existed}))?;
        }
        self.entries.push(Checkpoint { seq, task, file: file.to_path_buf(), hash, existed });
        Ok(())
    }

    /// Restore the last `n` checkpoints, newest first; returns restored paths.
    pub fn rewind(&mut self, n: usize) -> Result<Vec<PathBuf>> {
        let mut restored = Vec::new();
        for _ in 0..n {
            let Some(entry) = self.entries.pop() else { break };
            // Keep the index in step: the popped entry is gone from both.
            let kept: Vec<String> = std::fs::read_to_string(self.dir.join("index.jsonl"))
                .unwrap_or_default()
                .lines()
                .filter(|l| !l.contains(&format!("\"seq\":{}", entry.seq)))
                .map(str::to_string)
                .collect();
            let _ = std::fs::write(self.dir.join("index.jsonl"), kept.join("\n") + if kept.is_empty() { "" } else { "\n" });
            if entry.existed {
                let snap = std::fs::read(self.dir.join(format!("{:05}.snap", entry.seq)))?;
                std::fs::write(&entry.file, snap)?;
            } else {
                let _ = std::fs::remove_file(&entry.file);
            }
            restored.push(entry.file);
        }
        Ok(restored)
    }

    /// Oscillation probe: has this exact content been seen before for `file`
    /// this task? (§5.4)
    pub fn seen(&self, task: u32, file: &Path, hash: blake3::Hash) -> bool {
        self.entries
            .iter()
            .any(|e| e.task == task && e.file == file && e.hash == hash)
    }

}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_reopened_store_remembers_its_checkpoints() {
        let dir = crate::tools::testutil::tmp("ckpt-index");
        let id = uuid::Uuid::now_v7();
        let file = dir.join("a.txt");
        std::fs::write(&file, "one").unwrap();
        let mut store = Store::open(&dir, id).unwrap();
        store.snapshot(1, &file).unwrap();
        std::fs::write(&file, "two").unwrap();
        drop(store);
        // Same session, new process: the index restores the entries.
        let mut again = Store::open(&dir, id).unwrap();
        assert!(again.seen(1, &file, blake3::hash(b"one")));
        assert_eq!(again.rewind(1).unwrap(), vec![file.clone()]);
        assert_eq!(std::fs::read_to_string(&file).unwrap(), "one");
        assert!(Store::open(&dir, id).unwrap().rewind(1).unwrap().is_empty(), "index shrank with the rewind");
    }
}
