//! `decide` tool: calibrated probabilities from the decision model for
//! questions with a fixed answer set — which option, yes/no, or a rating.
//! Advice for the agent, not authority; see `crate::decide`.

use anyhow::{Result, bail};
use serde::Deserialize;
use serde_json::Value;
use std::collections::BTreeMap;

use crate::decide::Question;
use crate::tools::Toolbox;

#[derive(Deserialize)]
pub struct DecideArgs {
    /// The facts the decision rests on. The model sees nothing else.
    pub context: Value,
    pub questions: Vec<DecideQuestion>,
}

#[derive(Deserialize)]
pub struct DecideQuestion {
    pub id: Option<String>,
    pub question: String,
    /// Named options → a choice. A list uses the names as descriptions.
    pub options: Option<Value>,
    /// Ordered levels, worst to best or low to high → a score.
    pub scale: Option<Vec<String>>,
}

const MAX_QUESTIONS: usize = 16;

/// Tool arguments → typed questions. Pure, so it's testable without a model.
pub fn to_questions(args: &DecideArgs) -> Result<Vec<(String, Question)>> {
    if args.questions.is_empty() {
        bail!("no questions");
    }
    if args.questions.len() > MAX_QUESTIONS {
        bail!("at most {MAX_QUESTIONS} questions per call");
    }
    let mut out = Vec::new();
    for (i, q) in args.questions.iter().enumerate() {
        let id = q.id.clone().unwrap_or_else(|| format!("q{}", i + 1));
        if q.question.trim().is_empty() {
            bail!("question {id} is empty");
        }
        let question = match (&q.options, &q.scale) {
            (Some(_), Some(_)) => bail!("question {id}: give options or a scale, not both"),
            (Some(options), None) => {
                let opts = parse_options(options)
                    .ok_or_else(|| anyhow::anyhow!("question {id}: options must be a list of names, a list of {{name: description}} objects, or one {{name: description}} object"))?;
                if opts.len() < 2 {
                    bail!("question {id} needs at least two distinct options (got {})", opts.len());
                }
                Question::choice(&q.question, opts)
            }
            (None, Some(levels)) => {
                if levels.len() < 2 || levels.len() > 10 {
                    bail!("question {id}: a scale has 2 to 10 levels");
                }
                Question::score(&q.question, levels.clone())
            }
            (None, None) => Question::noul(&q.question),
        };
        out.push((id, question));
    }
    Ok(out)
}

/// Options arrive in whatever shape the model reached for: `["a","b"]`,
/// `{"a":"…","b":"…"}`, `[{"a":"…"},{"b":"…"}]`, or
/// `[{"name":"a","description":"…"}, …]`. Accept all of them.
fn parse_options(v: &Value) -> Option<BTreeMap<String, String>> {
    let mut out = BTreeMap::new();
    match v {
        Value::Object(map) => {
            for (k, d) in map {
                out.insert(k.clone(), d.as_str().map(str::to_string).unwrap_or_else(|| d.to_string()));
            }
        }
        Value::Array(items) => {
            for it in items {
                match it {
                    Value::String(s) => {
                        out.insert(s.clone(), s.clone());
                    }
                    Value::Object(m) => {
                        let name = ["name", "id", "option", "label", "key"].iter().find_map(|k| m.get(*k).and_then(Value::as_str));
                        match name {
                            Some(name) => {
                                let desc = ["description", "desc", "meaning", "value"]
                                    .iter()
                                    .find_map(|k| m.get(*k).and_then(Value::as_str))
                                    .unwrap_or(name);
                                out.insert(name.to_string(), desc.to_string());
                            }
                            // {"a": "description"} — one entry per object.
                            None => {
                                for (k, d) in m {
                                    out.insert(k.clone(), d.as_str().map(str::to_string).unwrap_or_else(|| d.to_string()));
                                }
                            }
                        }
                    }
                    other => {
                        out.insert(other.to_string(), other.to_string());
                    }
                }
            }
        }
        _ => return None,
    }
    Some(out)
}

pub async fn run(tb: &mut Toolbox, args: &Value) -> Result<String> {
    let Some(decider) = tb.decider.clone() else {
        bail!("no decision model configured ([decide] model in ~/.tursi/config.toml) — decide for yourself");
    };
    let args: DecideArgs = serde_json::from_value(args.clone())?;
    let questions = to_questions(&args)?;
    let subject: String = questions.first().map(|(_, q)| match q {
        Question::Noul { instructions, .. } | Question::Choice { instructions, .. } | Question::Score { instructions, .. } => {
            instructions.chars().take(80).collect()
        }
    }).unwrap_or_default();
    let decision = decider.ask(args.context, questions.clone()).await?;
    decider.record("tool", &subject, &decision);
    let mut out = format!("{} ({:.1} s):", decider.model().rsplit('/').next().unwrap_or("decide"), decision.elapsed.as_secs_f64());
    for (id, _) in &questions {
        match decision.answers.get(id) {
            Some(a) => out.push_str(&format!("\n- {id}: {}", a.render())),
            None => out.push_str(&format!("\n- {id}: no answer")),
        }
    }
    out.push_str("\nProbabilities, not verdicts: a close call means ask the user or look further.");
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn arguments_map_to_the_three_question_types() {
        let args: DecideArgs = serde_json::from_value(json!({
            "context": {"task": "rename a module"},
            "questions": [
                {"question": "Is this risky?"},
                {"id": "how", "question": "Which approach?", "options": ["sed", "edit tool", "rewrite"]},
                {"question": "How hard?", "scale": ["trivial", "easy", "hard"]},
                {"question": "Which model?", "options": {"flash": "cheap", "pro": "strong"}}
            ]
        }))
        .unwrap();
        let qs = to_questions(&args).unwrap();
        assert_eq!(qs[0].0, "q1");
        assert!(matches!(qs[0].1, Question::Noul { .. }));
        assert_eq!(qs[1].0, "how");
        assert!(matches!(&qs[1].1, Question::Choice { criteria, .. } if criteria.len() == 3 && criteria["sed"] == "sed"));
        assert!(matches!(&qs[2].1, Question::Score { criteria, .. } if criteria.len() == 3));
        assert!(matches!(&qs[3].1, Question::Choice { criteria, .. } if criteria["flash"] == "cheap"));

        // The shapes a model actually reaches for.
        let shapes: DecideArgs = serde_json::from_value(json!({"context": "x", "questions": [
            {"question": "?", "options": [{"a": "log crate"}, {"b": "tracing"}]},
            {"question": "?", "options": [{"name": "a", "description": "log crate"}, {"name": "b", "description": "tracing"}]}
        ]})).unwrap();
        for (_, q) in to_questions(&shapes).unwrap() {
            assert!(matches!(&q, Question::Choice { criteria, .. } if criteria["a"] == "log crate" && criteria["b"] == "tracing"), "{q:?}");
        }
        let bad: DecideArgs =
            serde_json::from_value(json!({"context": "x", "questions": [{"question": "?", "options": ["one"]}]})).unwrap();
        assert!(to_questions(&bad).unwrap_err().to_string().contains("two distinct options"));
        let both: DecideArgs =
            serde_json::from_value(json!({"context": "x", "questions": [{"question": "?", "options": ["a","b"], "scale": ["1","2"]}]})).unwrap();
        assert!(to_questions(&both).unwrap_err().to_string().contains("not both"));
    }
}
