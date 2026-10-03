//! read / write / edit: staleness hashes, checkpoints, per-file transactions
//! (§3, §4.1).

use anyhow::{Context, Result, anyhow, bail};
use serde::Deserialize;
use serde_json::Value;
use std::collections::HashMap;
use std::path::PathBuf;

use crate::bus::{EventKind, UiHandle};
use crate::tools::Toolbox;

/// Per-agent read-state: full-file staleness hash + what range is in context
/// (dedupe and coverage checks, §4).
#[derive(Default)]
pub struct State {
    pub reads: HashMap<PathBuf, ReadRecord>,
}

impl State {
    /// These files' content left the context (compaction): keep the staleness
    /// hashes, drop the dedupe so the next read sends the bytes again.
    pub fn forget(&mut self, paths: impl IntoIterator<Item = PathBuf>) {
        for path in paths {
            if let Some(rec) = self.reads.get_mut(&path) {
                rec.in_context = false;
            }
        }
    }
}

#[derive(Clone)]
pub struct ReadRecord {
    /// Hash of the ENTIRE file at read time — staleness is file-level even
    /// for ranged reads.
    pub file_hash: blake3::Hash,
    pub turn: u32,
    pub offset: u32,
    /// None = the whole file.
    pub limit: Option<u32>,
    /// The read's output is still in the transcript — only then may a re-read
    /// answer "unchanged" instead of sending bytes.
    pub in_context: bool,
}

/// Lines a `read` returns when the model gives no `limit` (§0.2: the model
/// asks for more by raising `limit` — the marker tells it how). Every line
/// sits in the append-only transcript for the rest of the task.
const DEFAULT_READ_LINES: u32 = 300;
/// Bytes per range, whatever `limit` says: a 2000-line read of long lines was
/// a 31k-token spike in practice. Continue with `offset`.
const MAX_READ_BYTES: usize = 32 * 1024;
/// Longer lines are cut — a minified bundle is one line.
const MAX_LINE_CHARS: usize = 2000;

#[derive(Deserialize)]
pub struct ReadArgs {
    pub reads: Vec<ReadRange>,
}

#[derive(Deserialize)]
pub struct ReadRange {
    pub file: PathBuf,
    pub offset: Option<u32>,
    pub limit: Option<u32>,
}

#[derive(Deserialize)]
pub struct EditArgs {
    pub edits: Vec<Hunk>,
}

#[derive(Deserialize)]
pub struct Hunk {
    pub file: PathBuf,
    pub old_string: String,
    pub new_string: String,
    pub replace_all: Option<bool>,
}

#[derive(Deserialize)]
pub struct WriteArgs {
    pub file: PathBuf,
    pub content: String,
}

/// Batched, ranged, line-numbered; explicit truncation marker; records the
/// staleness hash. Unchanged content whose range is already in context
/// returns `unchanged since turn N` instead of the bytes (§4).
pub async fn read(tb: &mut Toolbox, args: &Value) -> Result<String> {
    let args: ReadArgs = serde_json::from_value(args.clone())?;
    let mut out = String::new();
    for r in args.reads {
        // `log#N` is a log reference, not a file — models reach for `read`;
        // point them at the right tool instead of a confusing ENOENT.
        if let Some(name) = r.file.to_str() {
            if name.starts_with("log#") {
                anyhow::bail!("{name} is a log reference, not a file — use log_search to retrieve it");
            }
        }
        let path = tb.sandbox.resolve(&r.file, false)?;
        let content = std::fs::read_to_string(&path)
            .with_context(|| format!("cannot read {}", r.file.display()))?;
        let hash = blake3::hash(content.as_bytes());
        let lines: Vec<&str> = content.lines().collect();
        let total = lines.len() as u32;
        let offset = r.offset.unwrap_or(1).max(1);
        let limit = r.limit.unwrap_or(DEFAULT_READ_LINES).max(1);

        if let Some(rec) = tb.fs.reads.get(&path) {
            if rec.in_context && rec.file_hash == hash && covers(rec, offset, limit, total) {
                out.push_str(&format!("{}: unchanged since turn {}\n", r.file.display(), rec.turn));
                continue;
            }
        }

        out.push_str(&format!("── {} ──\n", r.file.display()));
        let start = (offset - 1) as usize;
        let mut end = start;
        let budget_start = out.len();
        // Numbers padded to the range's last line, not to five digits: on a
        // 300-line read the fixed padding alone was ~450 tokens.
        let w = (start + limit as usize).min(lines.len()).max(1).to_string().len();
        for (idx, line) in lines.iter().enumerate().skip(start).take(limit as usize) {
            if out.len() - budget_start >= MAX_READ_BYTES && end > start {
                break;
            }
            match line.char_indices().nth(MAX_LINE_CHARS) {
                Some((cut, _)) => out.push_str(&format!(
                    "{:>w$}→{} … [line cut at {MAX_LINE_CHARS} of {} chars]\n",
                    idx + 1,
                    &line[..cut],
                    line.chars().count()
                )),
                None => out.push_str(&format!("{:>w$}→{}\n", idx + 1, line)),
            }
            end = idx + 1;
        }
        if (end as u32) < total {
            out.push_str(&format!(
                "[truncated at line {end} of {total} — continue with offset={}, or raise limit (default {DEFAULT_READ_LINES}, {}KB max per range)]\n",
                end + 1,
                MAX_READ_BYTES / 1024
            ));
        }
        tb.fs.reads.insert(
            path,
            ReadRecord { file_hash: hash, turn: tb.turn, offset, limit: Some(limit), in_context: true },
        );
    }
    Ok(out.trim_end().to_string())
}

/// Does the recorded range cover the requested one?
fn covers(rec: &ReadRecord, offset: u32, limit: u32, total: u32) -> bool {
    let rec_end = rec.limit.map(|l| rec.offset.saturating_add(l) - 1).unwrap_or(total).min(total);
    let req_end = (offset.saturating_add(limit) - 1).min(total);
    rec.offset <= offset && rec_end >= req_end
}

/// New files are free (plus an "absent" checkpoint so rewind deletes them);
/// overwriting requires a prior, still-fresh read this session (§4.1).
pub async fn write(tb: &mut Toolbox, args: &Value, ui: &UiHandle) -> Result<String> {
    let args: WriteArgs = serde_json::from_value(args.clone())?;
    let path = tb.sandbox.resolve(&args.file, true)?;
    let old = if path.exists() {
        let rec = tb.fs.reads.get(&path).ok_or_else(|| {
            anyhow!("{} exists — read it before overwriting", args.file.display())
        })?;
        let current = std::fs::read_to_string(&path)?;
        if blake3::hash(current.as_bytes()) != rec.file_hash {
            bail!("{} changed since last read — re-read before writing", args.file.display());
        }
        current
    } else {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        String::new()
    };

    tb.checkpoints.snapshot(tb.task, &path)?;
    std::fs::write(&path, &args.content)?;
    ui.send(EventKind::FileDiff(crate::diff::diff(&args.file.display().to_string(), &old, &args.content))).await;
    let hash = blake3::hash(args.content.as_bytes());
    tb.fs.reads.insert(
        path.clone(),
        ReadRecord { file_hash: hash, turn: tb.turn, offset: 1, limit: None, in_context: true },
    );
    let mut line = format!("wrote {} ({} lines)", args.file.display(), args.content.lines().count());
    // Output tokens are the expensive ones: re-sending a whole file to change
    // a few lines is the single costliest habit. Say so, with numbers.
    if !old.is_empty() {
        let d = crate::diff::diff("", &old, &args.content);
        let total = args.content.lines().count();
        let changed = d.added.max(d.removed);
        if total >= 20 && changed * 3 < total {
            line.push_str(&format!(
                " — note: this write re-sent {} unchanged lines to change {changed}; use edit for changes like this",
                total - d.added
            ));
        }
    }
    // A freshly written file is a prime source of type/import errors (a whole
    // new module never typechecked) — run the same post-edit diagnostics (§3.1).
    let spawn = tb.lsp_check_edits;
    if let Ok(Some(diag)) = tb.lsp.diagnostics_after_edit(&path, spawn).await {
        line.push_str(&format!(" — {diag}"));
    }
    Ok(line)
}

/// Hunks grouped by file; each file's hunks validate together and apply as
/// one mutation — one checkpoint, one diagnostics run (§3.1). A failing hunk fails its file's transaction; files already
/// applied stand (independence per §3.1).
pub async fn edit(tb: &mut Toolbox, args: &Value, ui: &UiHandle) -> Result<String> {
    let args: EditArgs = serde_json::from_value(args.clone())?;
    let mut report: Vec<String> = Vec::new();
    for (file, hunks) in group_by_file(args.edits) {
        match apply_file_transaction(tb, &file, hunks, ui).await {
            Ok(line) => report.push(line),
            Err(e) => {
                let done = if report.is_empty() {
                    String::new()
                } else {
                    format!("{}; ", report.join("; "))
                };
                bail!("{done}{}: {e} — files already applied stand", file.display());
            }
        }
    }
    Ok(report.join("\n"))
}

/// Group hunks per file, preserving first-occurrence order.
fn group_by_file(hunks: Vec<Hunk>) -> Vec<(PathBuf, Vec<Hunk>)> {
    let mut groups: Vec<(PathBuf, Vec<Hunk>)> = Vec::new();
    for hunk in hunks {
        match groups.iter_mut().find(|(f, _)| *f == hunk.file) {
            Some((_, list)) => list.push(hunk),
            None => groups.push((hunk.file.clone(), vec![hunk])),
        }
    }
    groups
}

/// One file's transaction: stale-check → validate all hunks on a working
/// copy → checkpoint → write → diagnostics.
async fn apply_file_transaction(tb: &mut Toolbox, file: &PathBuf, hunks: Vec<Hunk>, ui: &UiHandle) -> Result<String> {
    let path = tb.sandbox.resolve(file, true)?;
    let original = std::fs::read_to_string(&path)
        .with_context(|| format!("cannot read {}", file.display()))?;
    let rec = tb
        .fs
        .reads
        .get(&path)
        .cloned()
        .ok_or_else(|| anyhow!("read it before editing"))?;
    if blake3::hash(original.as_bytes()) != rec.file_hash {
        bail!("changed since last read — re-read before editing");
    }

    // `read` shows lines without their `\r`, so the model writes LF hunks; in
    // a CRLF file, match and write them as CRLF.
    let crlf = is_crlf(&original);
    let mut work = original.clone();
    for (i, hunk) in hunks.iter().enumerate() {
        if hunk.old_string.is_empty() {
            bail!(
                "hunk {}: old_string is empty — include surrounding text to anchor the change \
                 (use write to create a file)",
                i + 1
            );
        }
        let (old, new) = if crlf {
            (to_crlf(&hunk.old_string), to_crlf(&hunk.new_string))
        } else {
            (hunk.old_string.clone(), hunk.new_string.clone())
        };
        let count = work.matches(&old).count();
        if hunk.replace_all.unwrap_or(false) {
            if count == 0 {
                bail!("hunk {}: old_string not found", i + 1);
            }
            work = work.replace(&old, &new);
        } else {
            match count {
                0 => bail!("hunk {}: old_string not found", i + 1),
                1 => work = work.replacen(&old, &new, 1),
                n => bail!("hunk {}: {n} matches — make old_string unique", i + 1),
            }
        }
    }

    let new_hash = blake3::hash(work.as_bytes());
    let oscillated = tb.checkpoints.seen(tb.task, &path, new_hash);

    tb.checkpoints.snapshot(tb.task, &path)?;
    std::fs::write(&path, &work)?;
    ui.send(EventKind::FileDiff(crate::diff::diff(&file.display().to_string(), &original, &work))).await;
    tb.fs.reads.insert(
        path.clone(),
        ReadRecord { file_hash: new_hash, turn: tb.turn, offset: rec.offset, limit: rec.limit, in_context: rec.in_context },
    );

    let mut line = format!("applied {} hunk(s) to {}", hunks.len(), file.display());
    if oscillated {
        line.push_str(" — note: result matches an earlier checkpoint (undo loop?)");
    }
    // The result as it now reads, numbered, so the model needn't re-read the
    // file to check its work — a re-read is a whole round trip.
    let region = changed_region(&original, &work);
    if !region.is_empty() {
        line.push('\n');
        line.push_str(&region);
    }
    // Post-edit diagnostics (§3.1). With lsp_check_edits, spawn the server on
    // first touch so type/import errors are caught as they're written.
    let spawn = tb.lsp_check_edits;
    if let Ok(Some(diag)) = tb.lsp.diagnostics_after_edit(&path, spawn).await {
        line.push_str(&format!(" — {diag}"));
    }
    Ok(line)
}

/// New-side context around each change, numbered like `read`: `CONTEXT`
/// lines either side, hunks joined by `…`, capped so a large edit stays a
/// glance (the diff view in the TUI has the rest).
fn changed_region(old: &str, new: &str) -> String {
    const CONTEXT: usize = 2;
    const MAX_LINES: usize = 40;
    let d = crate::diff::diff("", old, new);
    let new_lines: Vec<&str> = new.lines().collect();
    let mut show = vec![false; new_lines.len()];
    for l in d.lines.iter().filter(|l| l.kind == crate::diff::Kind::Added) {
        if let Some(n) = l.new_no {
            let i = n - 1;
            for flag in show.iter_mut().skip(i.saturating_sub(CONTEXT)).take(CONTEXT * 2 + 1) {
                *flag = true;
            }
        }
    }
    // A pure deletion: show where it happened.
    if !show.iter().any(|f| *f)
        && let Some(n) = d.lines.iter().find(|l| l.kind == crate::diff::Kind::Removed).and_then(|l| l.old_no)
    {
        let i = n.saturating_sub(1).min(new_lines.len().saturating_sub(1));
        for flag in show.iter_mut().skip(i.saturating_sub(CONTEXT)).take(CONTEXT * 2) {
            *flag = true;
        }
    }
    let w = new_lines.len().max(1).to_string().len();
    let mut out = String::new();
    let (mut shown, mut gap) = (0, false);
    for (i, line) in new_lines.iter().enumerate() {
        if !show[i] {
            gap = !out.is_empty();
            continue;
        }
        if shown == MAX_LINES {
            out.push_str("…\n");
            break;
        }
        if gap {
            out.push_str("…\n");
            gap = false;
        }
        out.push_str(&format!("{:>w$}→{}\n", i + 1, line));
        shown += 1;
    }
    out.trim_end().to_string()
}

/// Every line ending is `\r\n` (a mixed file is left to exact matching).
fn is_crlf(text: &str) -> bool {
    let crlf = text.matches("\r\n").count();
    crlf > 0 && crlf == text.matches('\n').count()
}

fn to_crlf(text: &str) -> String {
    text.replace("\r\n", "\n").replace('\n', "\r\n")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bus::{ROOT, UiEvent, UiHandle};
    use std::path::Path;
    use tokio::sync::mpsc;

    fn toolbox(dir: &Path) -> (Toolbox, UiHandle, mpsc::Receiver<UiEvent>) {
        let (tx, rx) = mpsc::channel(8);
        let sandbox = crate::sandbox::Sandbox::for_tests(dir);
        let tb = Toolbox {
            agent: ROOT,
            project: dir.to_path_buf(),
            fs: State::default(),
            sandbox: sandbox.clone(),
            lsp: crate::lsp::Manager::new(dir.to_path_buf(), Default::default(), sandbox.clone()),
            debugger: None,
        rizin: None,
            checkpoints: crate::checkpoint::Store::open(dir, uuid::Uuid::now_v7()).unwrap(),
            custom: crate::tools::custom::Registry { entries: vec![] },
            monitors: crate::monitor::Manager::new(sandbox.clone()).0,
            afk: false,
            lsp_check_edits: false,
            task: 1,
            turn: 1,
        };
        (tb, UiHandle { agent: ROOT, tx }, rx)
    }

    fn tmp(name: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("tursi-fs-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    async fn read_file(tb: &mut Toolbox, file: &str) -> String {
        read(tb, &serde_json::json!({"reads": [{"file": file}]})).await.unwrap()
    }

    #[tokio::test]
    async fn edit_fails_stale_when_file_changed_since_last_read() {
        let dir = tmp("stale");
        std::fs::write(dir.join("a.txt"), "hello world\n").unwrap();
        let (mut tb, ui, _rx) = toolbox(&dir);
        read_file(&mut tb, "a.txt").await;
        std::fs::write(dir.join("a.txt"), "changed externally\n").unwrap();
        let err = edit(
            &mut tb,
            &serde_json::json!({"edits": [{"file": "a.txt", "old_string": "hello", "new_string": "hi"}]}),
            &ui,
        )
        .await
        .unwrap_err();
        assert!(err.to_string().contains("changed since last read"));
    }

    #[tokio::test]
    async fn overwrite_without_prior_read_is_rejected() {
        let dir = tmp("overwrite");
        std::fs::write(dir.join("a.txt"), "precious\n").unwrap();
        let (mut tb, ui, _rx) = toolbox(&dir);
        let err = write(
            &mut tb,
            &serde_json::json!({"file": "a.txt", "content": "clobbered"}),
            &ui,
        )
        .await
        .unwrap_err();
        assert!(err.to_string().contains("read it before overwriting"));
        assert_eq!(std::fs::read_to_string(dir.join("a.txt")).unwrap(), "precious\n");
    }

    #[tokio::test]
    async fn ambiguous_old_string_is_rejected_with_match_count() {
        let dir = tmp("ambiguous");
        std::fs::write(dir.join("a.txt"), "foo bar foo\n").unwrap();
        let (mut tb, ui, _rx) = toolbox(&dir);
        read_file(&mut tb, "a.txt").await;
        let err = edit(
            &mut tb,
            &serde_json::json!({"edits": [{"file": "a.txt", "old_string": "foo", "new_string": "baz"}]}),
            &ui,
        )
        .await
        .unwrap_err();
        assert!(err.to_string().contains("2 matches"));
    }

    #[tokio::test]
    async fn one_bad_hunk_rolls_back_its_whole_file_transaction() {
        let dir = tmp("rollback");
        std::fs::write(dir.join("a.txt"), "alpha\nbeta\n").unwrap();
        let (mut tb, ui, _rx) = toolbox(&dir);
        read_file(&mut tb, "a.txt").await;
        let err = edit(
            &mut tb,
            &serde_json::json!({"edits": [
                {"file": "a.txt", "old_string": "alpha", "new_string": "ALPHA"},
                {"file": "a.txt", "old_string": "gamma", "new_string": "GAMMA"}
            ]}),
            &ui,
        )
        .await
        .unwrap_err();
        assert!(err.to_string().contains("hunk 2"));
        assert_eq!(std::fs::read_to_string(dir.join("a.txt")).unwrap(), "alpha\nbeta\n");
    }

    #[tokio::test]
    async fn crlf_files_take_lf_hunks_and_stay_crlf() {
        let dir = tmp("crlf");
        std::fs::write(dir.join("a.txt"), "one\r\ntwo\r\nthree\r\n").unwrap();
        let (mut tb, ui, _rx) = toolbox(&dir);
        let shown = read_file(&mut tb, "a.txt").await;
        assert!(!shown.contains('\r'), "read hides CRs");
        edit(
            &mut tb,
            &serde_json::json!({"edits": [{"file": "a.txt", "old_string": "one\ntwo", "new_string": "one\n1.5\ntwo"}]}),
            &ui,
        )
        .await
        .unwrap();
        assert_eq!(std::fs::read_to_string(dir.join("a.txt")).unwrap(), "one\r\n1.5\r\ntwo\r\nthree\r\n");
    }

    #[tokio::test]
    async fn empty_old_string_is_rejected_even_with_replace_all() {
        let dir = tmp("empty-old");
        std::fs::write(dir.join("a.txt"), "abc\n").unwrap();
        let (mut tb, ui, _rx) = toolbox(&dir);
        read_file(&mut tb, "a.txt").await;
        let err = edit(
            &mut tb,
            &serde_json::json!({"edits": [{"file": "a.txt", "old_string": "", "new_string": "X", "replace_all": true}]}),
            &ui,
        )
        .await
        .unwrap_err();
        assert!(err.to_string().contains("old_string is empty"), "{err}");
        assert_eq!(std::fs::read_to_string(dir.join("a.txt")).unwrap(), "abc\n");
    }

    #[tokio::test]
    async fn read_caps_lines_and_long_lines_by_default() {
        let dir = tmp("readcap");
        let mut big: String = (1..=2500).map(|i| format!("line {i}\n")).collect();
        big.push_str(&"x".repeat(5000));
        std::fs::write(dir.join("big.txt"), &big).unwrap();
        std::fs::write(dir.join("wide.txt"), "y".repeat(5000)).unwrap();
        let (mut tb, _ui, _rx) = toolbox(&dir);
        let out = read_file(&mut tb, "big.txt").await;
        assert!(out.contains("line 300") && !out.contains("line 301"), "default limit");
        assert!(out.contains("[truncated at line 300 of 2501 — continue with offset=301"), "marker: {}", &out[out.len() - 160..]);
        // Raising the limit works, but a range is still byte-capped.
        let out = read(&mut tb, &serde_json::json!({"reads": [{"file": "big.txt", "offset": 301, "limit": 5000}]})).await.unwrap();
        assert!(out.contains("line 2000"), "explicit limit honored");
        assert!(out.len() < 40_000, "byte cap: {}", out.len());
        let out = read_file(&mut tb, "wide.txt").await;
        assert!(out.contains("[line cut at 2000 of 5000 chars]"), "{}", &out[out.len() - 80..]);
        assert!(out.len() < 2100);
    }

    #[tokio::test]
    async fn forgotten_reads_resend_bytes_but_stay_edit_ready() {
        let dir = tmp("forget");
        std::fs::write(dir.join("a.txt"), "same content\n").unwrap();
        let (mut tb, ui, _rx) = toolbox(&dir);
        read_file(&mut tb, "a.txt").await;
        tb.fs.forget([dir.join("a.txt")]);
        let again = read_file(&mut tb, "a.txt").await;
        assert!(again.contains("same content"), "compacted-away content must be resent: {again}");
        tb.fs.forget([dir.join("a.txt")]);
        // Staleness survives forgetting: an edit still works without a re-read.
        edit(
            &mut tb,
            &serde_json::json!({"edits": [{"file": "a.txt", "old_string": "same", "new_string": "new"}]}),
            &ui,
        )
        .await
        .unwrap();
    }

    #[tokio::test]
    async fn edit_results_show_the_changed_region_numbered() {
        let dir = tmp("edit-region");
        let body: String = (1..=30).map(|i| format!("line {i}\n")).collect();
        std::fs::write(dir.join("a.txt"), &body).unwrap();
        let (mut tb, ui, _rx) = toolbox(&dir);
        let shown = read_file(&mut tb, "a.txt").await;
        assert!(shown.contains("\n 1→line 1\n") && shown.contains("\n30→line 30"), "range-width numbers: {shown}");
        let out = edit(
            &mut tb,
            &serde_json::json!({"edits": [
                {"file": "a.txt", "old_string": "line 5\n", "new_string": "line FIVE\nline 5b\n"},
                {"file": "a.txt", "old_string": "line 25\n", "new_string": "line TWENTY-FIVE\n"}
            ]}),
            &ui,
        )
        .await
        .unwrap();
        assert!(out.starts_with("applied 2 hunk(s) to a.txt\n"), "{out}");
        assert!(out.contains(" 3→line 3\n") && out.contains(" 5→line FIVE\n") && out.contains(" 6→line 5b\n") && out.contains(" 8→line 7"), "{out}");
        assert!(out.contains("…\n24→line 23\n25→line 24\n26→line TWENTY-FIVE\n"), "second hunk after a gap, renumbered: {out}");
        assert!(!out.contains("line 15"), "untouched middle stays out");
    }

    #[tokio::test]
    async fn a_write_that_mostly_resends_the_file_gets_the_edit_note() {
        let dir = tmp("write-waste");
        let body: String = (1..=40).map(|i| format!("line {i}\n")).collect();
        std::fs::write(dir.join("a.txt"), &body).unwrap();
        let (mut tb, ui, _rx) = toolbox(&dir);
        read_file(&mut tb, "a.txt").await;
        let out = write(&mut tb, &serde_json::json!({"file": "a.txt", "content": body.replace("line 7\n", "line seven\n")}), &ui).await.unwrap();
        assert!(out.contains("re-sent 39 unchanged lines to change 1"), "{out}");
        // A real rewrite (most lines change) gets no note.
        read_file(&mut tb, "a.txt").await;
        let out = write(&mut tb, &serde_json::json!({"file": "a.txt", "content": "totally\nnew\n"}), &ui).await.unwrap();
        assert!(!out.contains("re-sent"), "{out}");
    }

    #[tokio::test]
    async fn reread_of_unchanged_file_returns_dedupe_reference() {
        let dir = tmp("dedupe");
        std::fs::write(dir.join("a.txt"), "same content\n").unwrap();
        let (mut tb, ui, _rx) = toolbox(&dir);
        let _ = &ui;
        let first = read_file(&mut tb, "a.txt").await;
        assert!(first.contains("same content"));
        let second = read_file(&mut tb, "a.txt").await;
        assert!(second.contains("unchanged since turn 1"), "got: {second}");
        assert!(!second.contains("same content"));
    }
}
