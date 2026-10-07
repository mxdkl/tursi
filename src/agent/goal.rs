//! `/goal` — a completion condition the loop keeps working toward. After each
//! task end a separate evaluator call judges the condition from what the
//! model surfaced; not yet → the reason goes back in and the loop continues;
//! met or impossible → the goal clears. Session-scoped, persisted with the
//! transcript.

use anyhow::{Context, Result};
use std::time::Instant;
use tokio::sync::mpsc;

use crate::api::{ChatRequest, Message, Provider};

pub struct Goal {
    pub condition: String,
    pub started: Instant,
    /// Evaluations so far.
    pub turns: u32,
    pub last_reason: Option<String>,
    /// Consecutive evaluations after turns that used no tools — the model is
    /// arguing with the evaluator instead of working.
    pub idle: u32,
    /// Any tool ran since the last evaluation.
    pub worked: bool,
}

impl Goal {
    pub fn new(condition: String) -> Goal {
        Goal { condition, started: Instant::now(), turns: 0, last_reason: None, idle: 0, worked: false }
    }

    /// The instruction a goal starts with (and resumes with).
    pub fn directive(&self) -> String {
        format!(
            "[goal] Work until this condition holds, then end your turn stating the evidence \
             (what you ran, what it showed): {}",
            self.condition
        )
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Verdict {
    Met,
    NotYet,
    Impossible,
}

/// Evidence per evaluation: the last turns' text and tool results, capped.
const EVIDENCE_BYTES: usize = 8000;
/// Idle turns (no tools) before the loop stops and hands back control.
pub const IDLE_LIMIT: u32 = 3;

/// `MET: …` / `NOT_YET: …` / `IMPOSSIBLE: …` from the evaluator's reply.
pub fn parse_verdict(reply: &str) -> (Verdict, String) {
    for line in reply.lines().map(str::trim).filter(|l| !l.is_empty()) {
        let upper = line.to_ascii_uppercase();
        let (verdict, prefix) = if upper.starts_with("MET") && !upper.starts_with("METRIC") {
            (Verdict::Met, 3)
        } else if upper.starts_with("NOT_YET") || upper.starts_with("NOT YET") {
            (Verdict::NotYet, 7)
        } else if upper.starts_with("IMPOSSIBLE") {
            (Verdict::Impossible, 10)
        } else {
            continue;
        };
        let reason = line[prefix..].trim_start_matches([':', '-', ' ', '—']).trim().to_string();
        return (verdict, reason);
    }
    // An unparseable reply never ends a goal by accident.
    (Verdict::NotYet, format!("evaluator reply unclear: {}", reply.trim().chars().take(200).collect::<String>()))
}

/// What the evaluator gets to see: the final report and the tool results of
/// the last two assistant turns, newest last, cut to `EVIDENCE_BYTES`.
pub fn evidence(transcript: &[Message]) -> String {
    let mut turns = 0;
    let mut parts: Vec<String> = Vec::new();
    for m in transcript.iter().rev() {
        match m {
            Message::Assistant { text, tool_calls } => {
                turns += 1;
                if turns > 2 {
                    break;
                }
                if !text.trim().is_empty() {
                    parts.push(format!("agent: {}", text.trim()));
                }
                for c in tool_calls {
                    parts.push(format!("call: {}({})", c.name, c.arguments.to_string().chars().take(200).collect::<String>()));
                }
            }
            Message::ToolResult { content, is_error, .. } => {
                parts.push(format!("result{}: {}", if *is_error { " (error)" } else { "" }, content.trim()));
            }
            Message::User(text) if text.starts_with("[goal]") || text.starts_with("[verify]") => parts.push(format!("harness: {text}")),
            _ => {}
        }
    }
    parts.reverse();
    let mut out = parts.join("\n");
    if out.len() > EVIDENCE_BYTES {
        let cut = out.len() - EVIDENCE_BYTES;
        let start = out.char_indices().map(|(i, _)| i).find(|&i| i >= cut).unwrap_or(0);
        out = format!("…\n{}", &out[start..]);
    }
    out
}

/// One evaluator call. No tools: it judges only what the agent surfaced.
pub async fn evaluate(provider: &Provider, model: &str, condition: &str, evidence: &str) -> Result<(Verdict, String)> {
    let system = "You are a strict evaluator for an autonomous coding agent. You are given a completion \
                  condition and the agent's most recent turns (its words and its tool results). Decide \
                  whether the condition is demonstrably met by that evidence — claims without a shown \
                  check do not count. Reply with exactly one line: `MET: <reason>` if the evidence shows \
                  the condition holds; `IMPOSSIBLE: <reason>` only if it can never hold (not merely hard); \
                  otherwise `NOT_YET: <what is still missing or unproven, concretely>`.";
    let user = format!("Condition:\n{condition}\n\nEvidence:\n{evidence}");
    // Nobody streams the evaluator's reply: drop the receiver so sends fail
    // fast instead of blocking once the buffer fills.
    let (events, rx) = mpsc::channel(8);
    drop(rx);
    let turn = provider
        .chat(
            ChatRequest { model: model.to_string(), messages: vec![Message::System(system.to_string()), Message::User(user)], tools: vec![] },
            events,
        )
        .await
        .context("goal evaluator call")?;
    Ok(parse_verdict(&turn.text))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn verdicts_parse_leniently_and_never_pass_by_accident() {
        assert_eq!(parse_verdict("MET: cargo test exited 0 with 12 passed").0, Verdict::Met);
        assert_eq!(parse_verdict("not yet — the lint step was never run").0, Verdict::NotYet);
        assert_eq!(parse_verdict("Impossible: the file the condition names does not exist").0, Verdict::Impossible);
        assert_eq!(parse_verdict("Metrics look fine").0, Verdict::NotYet, "METRIC is not MET");
        let (v, reason) = parse_verdict("I think it is probably done");
        assert_eq!(v, Verdict::NotYet);
        assert!(reason.contains("unclear"));
        assert_eq!(parse_verdict("NOT_YET: tests pass but lint is red").1, "tests pass but lint is red");
    }

    #[test]
    fn evidence_keeps_the_last_two_turns_and_caps_size() {
        let mut t = vec![Message::User("fix it".into())];
        for i in 0..5 {
            t.push(Message::Assistant { text: format!("turn {i}"), tool_calls: vec![] });
            t.push(Message::ToolResult { call_id: "c".into(), content: "x".repeat(6000), is_error: false });
        }
        let e = evidence(&t);
        assert!(e.contains("turn 4") && !e.contains("turn 2"));
        assert!(e.len() <= EVIDENCE_BYTES + 8, "{}", e.len());
        assert!(e.starts_with('…'));
    }
}
