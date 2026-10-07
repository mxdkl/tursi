//! What a task is, for routing (§5.8): an activity (the kind of work), a
//! domain (what it works on), and a difficulty on a 0–4 scale (trivial, easy,
//! moderate, hard, very hard). The decision model labels every task at intake
//! in one call; nobody else picks them, so the lead never has to.
//!
//! The lists cover coding and the office, data, ops and research work in
//! Harness-Bench. Measured with Clef on its 106 tasks and 58 coding briefs:
//! an acceptable activity 95% and 91% of the time, an acceptable domain 97%
//! and 98%, identical labels across repeats. For difficulty the continuous
//! score (the probability-weighted level) is used, not the top level: Clef
//! squeezes the ends of the scale, and the expected level keeps the order
//! (Spearman 0.98 against hand labels on the coding briefs).

use serde::{Deserialize, Serialize};
use serde_json::json;
use std::collections::BTreeMap;

use crate::decide::{Answer, Decider, Question};

macro_rules! label_set {
    ($(#[$doc:meta])* $name:ident { $($variant:ident = $text:literal: $about:literal,)+ }) => {
        $(#[$doc])*
        #[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
        #[serde(rename_all = "lowercase")]
        pub enum $name { $($variant,)+ }

        impl $name {
            pub const ALL: &[$name] = &[$($name::$variant,)+];

            pub fn name(self) -> &'static str {
                match self { $($name::$variant => $text,)+ }
            }

            /// What the decision model reads when it picks this one.
            pub fn about(self) -> &'static str {
                match self { $($name::$variant => $about,)+ }
            }

            pub fn parse(s: &str) -> Option<$name> {
                $name::ALL.iter().copied().find(|v| v.name() == s.trim())
            }
        }
    };
}

label_set! {
    /// The kind of work a task asks for.
    Activity {
        New = "new": "write new code: a new module, feature, tool or project",
        Change = "change": "change existing behavior or swap a technology: extend a feature, add an option, migrate or port to another library, database or API",
        Debug = "debug": "find and fix a bug in behavior: a crash, wrong output or failing test (not a typo in text)",
        Test = "test": "write or fix tests",
        Refactor = "refactor": "restructure the same code without changing its behavior or technology: rename, split, move, deduplicate",
        Optimize = "optimize": "make something faster or smaller without changing what it does",
        Review = "review": "judge code, a change, a document or a request against quality standards, rules or policy, and give a verdict",
        Research = "research": "answer a question by reading code, logs, binaries, documents or web pages, without producing anything but the answer",
        Analyze = "analyze": "work out results from given data, records or documents (compute, classify, reconcile, detect, compare) and write them out as a report, table or JSON",
        Process = "process": "do a file or data chore directly: convert formats, clean, rename, extract, merge, deduplicate, or run commands and save their output",
        Plan = "plan": "make a plan, schedule or decision: break work into steps, assign people and times, resolve conflicts, replan when facts change",
        Writing = "writing": "write or edit prose for people: documentation, READMEs, comments, memos, emails, replies, summaries, briefs, release notes, including typo fixes",
        Build = "build": "builds, dependencies, versions, CI, packaging, tooling and configuration",
    }
}

label_set! {
    /// What a task works on.
    Domain {
        Systems = "systems": "low-level and performance code: Rust, C, C++, concurrency, memory, operating systems, compilers, emulators and JITs",
        Backend = "backend": "server-side application code: services, APIs, business logic, libraries",
        Frontend = "frontend": "user interface code: HTML, CSS, browser JavaScript, visual design",
        Data = "data": "data engineering: databases, SQL, schemas, migrations, data formats and pipelines",
        Analytics = "analytics": "analyzing data for answers: statistics, metrics, dashboards, funnels, forecasts, experiments and anomalies",
        Scripting = "scripting": "small standalone scripts, shell commands and command-line tools",
        Infra = "infra": "build systems, CI, packaging, deployment, containers, toolchains and config files",
        Ops = "ops": "running production services: incidents, logs, alerts, monitoring, capacity, releases and rollbacks",
        Binary = "binary": "existing binaries: reverse engineering, disassembly, firmware and file-format analysis",
        Security = "security": "vulnerabilities, exploits, fuzzing, secrets, permissions, integrity checks and attacks such as prompt injection",
        Browser = "browser": "using websites and web services as a client, not writing code for them: opening pages, reading or filling forms, scraping, calling HTTP APIs",
        Multimodal = "multimodal": "images, audio and video: recognizing, describing, editing or generating them",
        Office = "office": "office and business work: email, calendars, meetings, slides, Word and PDF documents, customer support, HR, sales, marketing, operations planning",
        Finance = "finance": "money: accounting, budgets, expenses, invoices, payments, reconciliation, pricing and financial checks such as KYC",
        Legal = "legal": "contracts, policies, regulations, compliance, privacy and appeals",
        Prose = "prose": "documentation, articles, research papers and other text, with their sources, citations and claims",
    }
}

/// A task's labels; any of them may be missing (no decision model, or the
/// call failed), and routing then leans on what is known.
#[derive(Clone, Copy, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Labels {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub activity: Option<Activity>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub domain: Option<Domain>,
    /// 0–4; None is routed as moderate.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub difficulty: Option<f64>,
}

impl Labels {
    /// `analyze·finance` for reports and the UI; `task` when unlabelled.
    pub fn name(&self) -> String {
        match (self.activity, self.domain) {
            (Some(a), Some(d)) => format!("{}·{}", a.name(), d.name()),
            (Some(a), None) => a.name().to_string(),
            (None, Some(d)) => d.name().to_string(),
            (None, None) => "task".to_string(),
        }
    }
}

const ACTIVITY: &str = "What kind of work does this task mainly ask for?";
const DOMAIN: &str = "What is this task mainly working on? Judge by the thing being made or changed, not the topic it \
    serves: code that handles payments is backend, a README for a fuzzer is prose.";
const DIFFICULTY: &str = "How hard is this task for an AI agent working in the project? Consider how much code or \
    material must be read and changed, how much is unknown, and how likely a first attempt is to fail.";
const LEVELS: [&str; 5] = ["trivial", "easy", "moderate", "hard", "very hard"];
/// Characters of a brief the model reads.
const BRIEF_CHARS: usize = 6000;

fn questions() -> Vec<(String, Question)> {
    vec![
        ("activity".into(), Question::choice(ACTIVITY, Activity::ALL.iter().map(|a| (a.name().to_string(), a.about().to_string())))),
        ("domain".into(), Question::choice(DOMAIN, Domain::ALL.iter().map(|d| (d.name().to_string(), d.about().to_string())))),
        ("difficulty".into(), Question::score(DIFFICULTY, LEVELS.iter().map(|s| s.to_string()).collect())),
    ]
}

fn from_answers(answers: &BTreeMap<String, Answer>) -> Labels {
    let choice = |k: &str| answers.get(k).and_then(|a| a.choice.clone());
    Labels {
        activity: choice("activity").and_then(|c| Activity::parse(&c)),
        domain: choice("domain").and_then(|c| Domain::parse(&c)),
        difficulty: answers.get("difficulty").and_then(|a| a.score).map(|s| s.clamp(0.0, 4.0)),
    }
}

/// Label a brief; all None without a decision model or on failure.
pub async fn label(decider: Option<&Decider>, brief: &str) -> Labels {
    let Some(decider) = decider else { return Labels::default() };
    let state = json!({ "task_brief": brief.chars().take(BRIEF_CHARS).collect::<String>() });
    match decider.ask(state, questions()).await {
        Ok(d) => {
            decider.record("labels", &brief.lines().next().unwrap_or("").chars().take(60).collect::<String>(), &d);
            from_answers(&d.answers)
        }
        Err(e) => {
            tracing::warn!("deals: labelling failed, routing as an unlabelled moderate task: {e:#}");
            Labels::default()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_round_trip_and_answers_become_labels() {
        for a in Activity::ALL {
            assert_eq!(Activity::parse(a.name()), Some(*a));
        }
        for d in Domain::ALL {
            assert_eq!(Domain::parse(d.name()), Some(*d));
        }
        assert_eq!(Activity::parse("cooking"), None);
        let answer = |choice: Option<&str>, score: Option<f64>| Answer {
            kind: "x".into(),
            noul: None,
            choice: choice.map(String::from),
            score,
            probabilities: None,
            legend: None,
            confidence: None,
        };
        let answers = BTreeMap::from([
            ("activity".to_string(), answer(Some("analyze"), None)),
            ("domain".to_string(), answer(Some("finance"), None)),
            ("difficulty".to_string(), answer(None, Some(4.6))),
        ]);
        let l = from_answers(&answers);
        assert_eq!((l.activity, l.domain, l.difficulty), (Some(Activity::Analyze), Some(Domain::Finance), Some(4.0)));
        assert_eq!(l.name(), "analyze·finance");
        assert_eq!(Labels::default().name(), "task");
        let json = serde_json::to_string(&l).unwrap();
        assert_eq!(json, r#"{"activity":"analyze","domain":"finance","difficulty":4.0}"#);
        assert_eq!(serde_json::to_string(&Labels::default()).unwrap(), "{}");
    }
}
