//! The QA judge (§5.8): when a task finishes, the decision model reads the
//! evidence (brief, how the run ended, the commands the subagent ran with
//! their exit marks, the files it changed, and its report) and gives the
//! probability that the brief was accomplished. That probability is the
//! verified outcome DEALS learns from, as fractional counts, and the lead
//! sees it on the report. With no decision model the ending alone sets it.

use serde_json::json;

use super::Activity;
use crate::api::Message;
use crate::decide::{Decider, Question};

/// How the task's last segment ended.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Ending {
    /// The subagent gave its report.
    Done,
    /// Out of turns with no splits left: the report is a last state.
    TurnLimit,
    /// The model call failed or the run errored.
    Failed,
    /// Esc: nothing to learn from.
    Interrupted,
}

impl Ending {
    pub fn name(self) -> &'static str {
        match self {
            Ending::Done => "finished and reported",
            Ending::TurnLimit => "stopped at its turn limit before finishing",
            Ending::Failed => "failed",
            Ending::Interrupted => "interrupted",
        }
    }

    /// The success estimate when no judge is available (or it fails).
    pub fn prior(self) -> f64 {
        match self {
            Ending::Done => 0.7,
            Ending::TurnLimit => 0.2,
            Ending::Failed | Ending::Interrupted => 0.0,
        }
    }
}

pub struct Evidence {
    pub activity: Option<Activity>,
    /// Declared no write areas: an answer is the whole deliverable.
    pub read_only: bool,
    pub brief: String,
    pub ending: Ending,
    pub report: String,
    /// `2 ✗ cargo test (3.1s)` lines, newest last.
    pub commands: Vec<String>,
    pub files: Vec<String>,
    /// The project root, to read what the changed files now hold.
    pub project: std::path::PathBuf,
}

/// How much of the changed files the judge reads: so many files, each
/// whole up to `FILE_WHOLE` characters or else its head, and no more in all.
const FILES_SHOWN: usize = 6;
const FILE_WHOLE: usize = 2500;
const FILE_HEAD: usize = 1500;
const FILES_TOTAL: usize = 9000;

/// What the changed files hold now, for the judge to check against the
/// brief: a deliverable's real content says more than any report about it.
pub fn contents(project: &std::path::Path, files: &[String]) -> Vec<serde_json::Value> {
    let mut out = Vec::new();
    let mut total = 0;
    for f in files.iter().take(FILES_SHOWN) {
        let path = if std::path::Path::new(f).is_absolute() { std::path::PathBuf::from(f) } else { project.join(f) };
        let content = match std::fs::read(&path) {
            Err(_) => "(deleted)".to_string(),
            Ok(bytes) => match String::from_utf8(bytes) {
                Err(e) => format!("(binary, {} bytes)", e.as_bytes().len()),
                Ok(text) if text.chars().count() <= FILE_WHOLE => text,
                Ok(text) => {
                    let n = text.chars().count();
                    format!("{}\n… ({} more characters)", clip(&text, FILE_HEAD), n - FILE_HEAD)
                }
            },
        };
        total += content.len();
        if total > FILES_TOTAL {
            break;
        }
        out.push(json!({ "path": f, "content": content }));
    }
    out
}

const MAX_COMMANDS: usize = 20;

fn clip(s: &str, n: usize) -> String {
    s.chars().take(n).collect()
}

/// Commands run and files written, read from a subagent's transcript.
pub fn trace(transcript: &[Message]) -> (Vec<String>, Vec<String>) {
    let mut commands = Vec::new();
    let mut files: Vec<String> = Vec::new();
    for m in transcript {
        match m {
            Message::Assistant { tool_calls, .. } => {
                for c in tool_calls.iter().filter(|c| matches!(c.name.as_str(), "edit" | "write")) {
                    let mut names: Vec<String> = c.arguments.get("file").and_then(|f| f.as_str()).map(String::from).into_iter().collect();
                    if let Some(edits) = c.arguments.get("edits").and_then(|e| e.as_array()) {
                        names.extend(edits.iter().filter_map(|e| e.get("file").and_then(|f| f.as_str()).map(String::from)));
                    }
                    for n in names {
                        if !files.contains(&n) {
                            files.push(n);
                        }
                    }
                }
            }
            Message::ToolResult { content, .. } => {
                for line in content.lines() {
                    let mut chars = line.trim_start().chars().skip_while(char::is_ascii_digit);
                    if chars.next() == Some(' ') && matches!(chars.next(), Some('✓' | '✗')) {
                        commands.push(clip(line.trim(), 160));
                    }
                }
            }
            _ => {}
        }
    }
    if commands.len() > MAX_COMMANDS {
        commands.drain(..commands.len() - MAX_COMMANDS);
    }
    (commands, files)
}

/// Probability the task succeeded.
pub async fn judge(decider: Option<&Decider>, ev: &Evidence) -> f64 {
    let prior = ev.ending.prior();
    if matches!(ev.ending, Ending::Failed | Ending::Interrupted) {
        return prior;
    }
    let Some(decider) = decider else { return prior };
    let state = json!({
        "activity": ev.activity.map(Activity::name),
        "read_only": ev.read_only,
        "brief": clip(&ev.brief, 3000),
        "how_it_ended": ev.ending.name(),
        "commands_run": ev.commands,
        "files_changed": ev.files,
        "file_contents": contents(&ev.project, &ev.files),
        "report": clip(&ev.report, 4000),
    });
    let question = Question::noul(
        "A subagent was given the brief and has finished. Judging only by the evidence (the commands it ran \
         and whether they passed (✓) or failed (✗), the files it changed and what they now contain, and its report), \
         did it accomplish what the brief asked, completely and correctly? Check the file contents against the brief's \
         requirements (names, formats, columns, fields, values, sort order): a missing or wrong detail counts against \
         it even when the report says otherwise. A report that claims success with no supporting command or change \
         counts against it; for read-only tasks, a specific, well-grounded answer to the question counts for it.",
    );
    match decider.ask(state, vec![("accomplished".into(), question)]).await {
        Ok(d) => {
            decider.record("qa", &format!("{} {}", ev.activity.map_or("task", Activity::name), clip(ev.brief.lines().next().unwrap_or(""), 60)), &d);
            let p = d.answers.get("accomplished").and_then(|a| a.noul).unwrap_or(prior);
            // An unfinished run can be good partial work, never a full success.
            if ev.ending == Ending::TurnLimit { p.min(0.4) } else { p }
        }
        Err(e) => {
            tracing::warn!("deals: QA judge failed, using the ending's prior: {e:#}");
            prior
        }
    }
}

#[cfg(test)]
mod contents_tests {
    use super::*;

    #[test]
    fn the_judge_reads_small_files_whole_and_big_ones_by_their_head() {
        let dir = crate::tools::testutil::tmp("qa-contents");
        std::fs::create_dir_all(dir.join("out")).unwrap();
        std::fs::write(dir.join("out/a.csv"), "id,name\n1,x\n").unwrap();
        std::fs::write(dir.join("big.txt"), "y".repeat(5000)).unwrap();
        std::fs::write(dir.join("b.bin"), [0u8, 159, 146, 150]).unwrap();
        let files = ["out/a.csv", "big.txt", "b.bin", "gone.txt"].map(String::from).to_vec();
        let c = contents(&dir, &files);
        assert_eq!(c[0]["content"], "id,name\n1,x\n");
        let big = c[1]["content"].as_str().unwrap();
        assert!(big.starts_with(&"y".repeat(FILE_HEAD)) && big.ends_with("(3500 more characters)"), "{}", &big[big.len() - 40..]);
        assert_eq!(c[2]["content"], "(binary, 4 bytes)");
        assert_eq!(c[3]["content"], "(deleted)");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::ToolCall;

    #[test]
    fn trace_reads_commands_and_changed_files() {
        let t = vec![
            Message::Assistant {
                text: String::new(),
                tool_calls: vec![
                    ToolCall { id: "1".into(), name: "edit".into(), arguments: json!({"edits": [{"file": "src/a.rs"}, {"file": "src/b.rs"}]}), malformed: None },
                    ToolCall { id: "2".into(), name: "write".into(), arguments: json!({"file": "src/a.rs"}), malformed: None },
                ],
            },
            Message::ToolResult { call_id: "3".into(), content: "1 ✓ cargo build (4.1s)\n2 ✗ cargo test (2.0s) [full: log#9]\n   error[E0425]".into(), is_error: false },
        ];
        let (commands, files) = trace(&t);
        assert_eq!(files, vec!["src/a.rs", "src/b.rs"]);
        assert_eq!(commands, vec!["1 ✓ cargo build (4.1s)", "2 ✗ cargo test (2.0s) [full: log#9]"]);
    }

    #[tokio::test]
    async fn without_a_judge_the_ending_decides() {
        let ev = |ending| Evidence { activity: Some(Activity::Writing), read_only: false, project: std::env::temp_dir(), brief: "b".into(), ending, report: "r".into(), commands: vec![], files: vec![] };
        assert_eq!(judge(None, &ev(Ending::Done)).await, 0.7);
        assert_eq!(judge(None, &ev(Ending::TurnLimit)).await, 0.2);
        assert_eq!(judge(None, &ev(Ending::Failed)).await, 0.0);
    }
}
