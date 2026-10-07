//! Decision models (Clef, Clef-flash, Jev): calibrated probabilities for typed
//! questions over a `state`, no text generation. One call answers many
//! questions in ~50–500 ms for a fraction of a cent. Used by the `decide` tool
//! and, in shadow mode, by the command-risk gate, whose decisions are only
//! logged to `~/.tursi/decisions.jsonl` so thresholds can be fitted on real
//! traffic before anything changes behavior. DEALS (§5.8) uses one to label
//! tasks and judge outcomes; which model runs a task it never asks.

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use crate::config::{Config, Secrets};

/// A typed question. Option order is alphabetical (BTreeMap) on purpose:
/// these models are sensitive to option order, so it must be deterministic.
#[derive(Clone, Debug, Serialize, PartialEq)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum Question {
    /// Yes/no → one probability.
    Noul {
        instructions: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        criteria: Option<BTreeMap<String, String>>,
    },
    /// One of named options → a distribution.
    Choice { instructions: String, criteria: BTreeMap<String, String> },
    /// Ordered levels → a distribution and an expected level.
    Score { instructions: String, criteria: Vec<String> },
}

impl Question {
    pub fn noul(instructions: impl Into<String>) -> Question {
        Question::Noul { instructions: instructions.into(), criteria: None }
    }
    pub fn choice(instructions: impl Into<String>, options: impl IntoIterator<Item = (String, String)>) -> Question {
        Question::Choice { instructions: instructions.into(), criteria: options.into_iter().collect() }
    }
    pub fn score(instructions: impl Into<String>, levels: Vec<String>) -> Question {
        Question::Score { instructions: instructions.into(), criteria: levels }
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct Answer {
    #[serde(rename = "type")]
    pub kind: String,
    pub noul: Option<f64>,
    pub choice: Option<String>,
    pub score: Option<f64>,
    pub probabilities: Option<BTreeMap<String, f64>>,
    pub legend: Option<BTreeMap<String, String>>,
    pub confidence: Option<f64>,
}

/// Two top options within this of each other read as a coin flip.
const CLOSE_CALL: f64 = 0.15;

impl Answer {
    /// Probability of "yes" for a noul question.
    #[cfg_attr(not(test), allow(dead_code))]
    pub fn yes(&self) -> Option<f64> {
        self.noul
    }

    /// One line a model (or a person) can act on.
    pub fn render(&self) -> String {
        match self.kind.as_str() {
            "noul" => {
                let p = self.noul.unwrap_or(0.0);
                let word = if p >= 0.5 { "yes" } else { "no" };
                let hedge = if (p - 0.5).abs() < 0.2 { " (close call)" } else { "" };
                format!("{word} {p:.2}{hedge}")
            }
            "choice" => {
                let mut ranked: Vec<(&String, &f64)> = self.probabilities.iter().flatten().collect();
                ranked.sort_by(|a, b| b.1.partial_cmp(a.1).unwrap_or(std::cmp::Ordering::Equal));
                let list = ranked.iter().map(|(k, p)| format!("{k} {p:.2}")).collect::<Vec<_>>().join(", ");
                let close = ranked.len() >= 2 && (ranked[0].1 - ranked[1].1) < CLOSE_CALL;
                format!("{} → {list}{}", self.choice.as_deref().unwrap_or("?"), if close { " (close call)" } else { "" })
            }
            "score" => {
                let s = self.score.unwrap_or(0.0);
                let legend = self.legend.clone().unwrap_or_default();
                let n = legend.len().max(1);
                let nearest = legend.get(&format!("{}", s.round() as i64)).cloned().unwrap_or_default();
                let dist = self
                    .probabilities
                    .iter()
                    .flatten()
                    .map(|(k, p)| format!("{} {p:.2}", legend.get(k).cloned().unwrap_or_else(|| k.clone())))
                    .collect::<Vec<_>>()
                    .join(", ");
                format!("{nearest} ({s:.1} of 0..{}) — {dist}", n - 1)
            }
            other => format!("{other}: {}", serde_json::to_string(self).unwrap_or_default()),
        }
    }
}

pub struct Decision {
    pub answers: BTreeMap<String, Answer>,
    pub input_tokens: u64,
    pub elapsed: Duration,
}

pub struct Decider {
    http: reqwest::Client,
    token: String,
    /// `…/ai/run` — Cloudflare's model-run endpoint, derived from the
    /// provider's chat base URL.
    run_url: String,
    /// Model id without the provider prefix: `@cf/cloudflare/clef-flash`,
    /// `typesafe/jev`.
    model: String,
    headers: Vec<(String, String)>,
    /// Shadow decisions (gate, routing) are made and logged but never acted on.
    pub shadow: bool,
    log: Option<PathBuf>,
}

impl Decider {
    /// None when no decision model is configured (`[decide] model = …`).
    pub fn from_config(config: &Config, secrets: &Secrets) -> Option<Decider> {
        let full = config.decide.model.clone()?;
        let (token, base) = secrets.provider_for(&full)?;
        let model = full.split_once('/').map(|(_, m)| m.to_string()).unwrap_or_else(|| full.clone());
        let run_url = format!("{}/run", base.trim_end_matches('/').trim_end_matches("/v1"));
        let log = Config::home_dir().ok().map(|h| h.join("decisions.jsonl"));
        Some(Decider {
            http: reqwest::Client::builder().timeout(Duration::from_secs(8)).build().ok()?,
            token,
            run_url,
            model,
            headers: secrets.headers_for(&full),
            shadow: config.decide.shadow,
            log,
        })
    }

    pub fn model(&self) -> &str {
        &self.model
    }

    /// Cloudflare-hosted models take the model in the path and a flat body;
    /// third-party ones (Jev) go to the bare run endpoint with the model in
    /// the body and the payload under `input`.
    fn request(&self, state: &Value, questions: &[(String, Question)]) -> (String, Value) {
        let qs: serde_json::Map<String, Value> =
            questions.iter().map(|(id, q)| (id.clone(), serde_json::to_value(q).unwrap_or(Value::Null))).collect();
        if self.model.starts_with("@cf/") {
            let short = self.model.rsplit('/').next().unwrap_or(&self.model);
            (format!("{}/{}", self.run_url, self.model), json!({"model": short, "state": state, "questions": qs}))
        } else {
            (self.run_url.clone(), json!({"model": self.model, "input": {"state": state, "questions": qs}}))
        }
    }

    fn parse(body: Value) -> Result<(BTreeMap<String, Answer>, u64)> {
        let mut result = body.get("result").cloned().unwrap_or(body);
        // Third-party models nest once more: {state: "Completed", result: {…}}.
        if result.get("answers").is_none() {
            if let Some(inner) = result.get("result").cloned() {
                result = inner;
            }
        }
        let answers = result.get("answers").cloned().ok_or_else(|| anyhow::anyhow!("no answers in reply"))?;
        let answers: BTreeMap<String, Answer> = serde_json::from_value(answers).context("decision answers")?;
        let tokens = result.pointer("/usage/input_tokens").and_then(Value::as_u64).unwrap_or(0);
        Ok((answers, tokens))
    }

    pub async fn ask(&self, state: Value, questions: Vec<(String, Question)>) -> Result<Decision> {
        if questions.is_empty() {
            bail!("no questions");
        }
        let (url, body) = self.request(&state, &questions);
        let started = Instant::now();
        let mut req = self.http.post(&url).bearer_auth(&self.token).header("User-Agent", "tursi/0.1");
        for (k, v) in &self.headers {
            req = req.header(k, v);
        }
        let resp = req.json(&body).send().await.context("decision request")?;
        let status = resp.status();
        let text = resp.text().await.unwrap_or_default();
        if !status.is_success() {
            bail!("decision model {}: HTTP {status}: {}", self.model, crate::output::redact(&text).chars().take(300).collect::<String>());
        }
        let (answers, input_tokens) = Self::parse(serde_json::from_str(&text).context("decision reply")?)?;
        Ok(Decision { answers, input_tokens, elapsed: started.elapsed() })
    }

    /// Append one line to the decisions log — the calibration data set.
    pub fn record(&self, kind: &str, subject: &str, decision: &Decision) {
        let Some(path) = &self.log else { return };
        let line = json!({
            "ts": chrono::Utc::now(),
            "kind": kind,
            "model": self.model,
            "subject": subject,
            "ms": decision.elapsed.as_millis() as u64,
            "input_tokens": decision.input_tokens,
            "answers": decision.answers,
        });
        if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(path) {
            use std::io::Write;
            let _ = writeln!(f, "{line}");
        }
    }

    /// Fire-and-forget: decide, log, never block or change the caller's path.
    pub fn shadow_ask(self: &Arc<Self>, kind: &'static str, subject: String, state: Value, questions: Vec<(String, Question)>) {
        if !self.shadow {
            return;
        }
        let me = self.clone();
        tokio::spawn(async move {
            match me.ask(state, questions).await {
                Ok(d) => {
                    let summary = d.answers.iter().map(|(k, a)| format!("{k}={}", a.render())).collect::<Vec<_>>().join("; ");
                    tracing::info!(target: "decide", kind, subject = %subject, ms = d.elapsed.as_millis() as u64, %summary, "shadow decision");
                    me.record(kind, &subject, &d);
                }
                Err(e) => tracing::warn!(target: "decide", kind, "shadow decision failed: {e:#}"),
            }
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn decider(model: &str) -> Decider {
        Decider {
            http: reqwest::Client::new(),
            token: "t".into(),
            run_url: "https://api.cloudflare.com/client/v4/accounts/abc/ai/run".into(),
            model: model.into(),
            headers: vec![],
            shadow: true,
            log: None,
        }
    }

    #[test]
    fn hosted_and_third_party_models_take_different_request_shapes() {
        let qs = vec![("risky".to_string(), Question::noul("Is it risky?"))];
        let (url, body) = decider("@cf/cloudflare/clef-flash").request(&json!("state"), &qs);
        assert!(url.ends_with("/ai/run/@cf/cloudflare/clef-flash"));
        assert_eq!(body["model"], "clef-flash");
        assert_eq!(body["questions"]["risky"]["type"], "noul");
        let (url, body) = decider("typesafe/jev").request(&json!("state"), &qs);
        assert!(url.ends_with("/ai/run"));
        assert_eq!(body["model"], "typesafe/jev");
        assert_eq!(body["input"]["questions"]["risky"]["instructions"], "Is it risky?");
    }

    #[test]
    fn replies_parse_in_both_nestings_and_render() {
        // Clef, as returned 2026-10-04.
        let clef = json!({"result": {"model": "clef", "answers": {
            "model": {"type": "choice", "choice": "flash", "probabilities": {"flash": 0.4837, "glm53flash": 0.4087, "glm53": 0.1076}, "confidence": 0.1188},
            "relevant": {"type": "noul", "noul": 0.6324},
            "difficulty": {"type": "score", "score": 1.98, "legend": {"0": "trivial", "1": "easy", "2": "moderate"}, "probabilities": {"0": 0.02, "1": 0.23, "2": 0.75}, "confidence": 0.23}},
            "usage": {"input_tokens": 660, "output_tokens": 0}}, "success": true});
        let (answers, tokens) = Decider::parse(clef).unwrap();
        assert_eq!(tokens, 660);
        assert!(answers["model"].render().starts_with("flash → flash 0.48, glm53flash 0.41, glm53 0.11 (close call)"));
        assert_eq!(answers["relevant"].render(), "yes 0.63 (close call)");
        assert!(answers["difficulty"].render().starts_with("moderate (2.0 of 0..2)"));
        // Jev via Cloudflare nests once more and reports no usage.
        let jev = json!({"result": {"state": "Completed", "result": {"model": "jev-1.13.0", "answers": {"c1": {"type": "noul", "noul": 0.91}}}}});
        let (answers, tokens) = Decider::parse(jev).unwrap();
        assert_eq!(tokens, 0);
        assert_eq!(answers["c1"].render(), "yes 0.91");
    }

    /// Live calibration check against the labelled set — needs credentials
    /// and a `[decide]` model: `cargo test live_labelled -- --ignored --nocapture`.
    #[tokio::test]
    #[ignore]
    async fn live_labelled_decisions() {
        let mut config = Config::load(std::path::Path::new(".")).unwrap();
        if let Ok(m) = std::env::var("TURSI_DECIDE_MODEL") {
            config.decide.model = Some(m);
        }
        let secrets = Secrets::load().unwrap();
        let d = Decider::from_config(&config, &secrets).expect("[decide] model configured");
        let set: Value = serde_json::from_str(&std::fs::read_to_string("bench/decisions.json").unwrap()).unwrap();
        let mut total = 0.0;
        let mut correct = 0;
        let mut n = 0;
        for group in set["groups"].as_array().unwrap() {
            let items = group["items"].as_array().unwrap();
            let qs: Vec<(String, Question)> = items
                .iter()
                .enumerate()
                .map(|(i, it)| (format!("q{i}"), Question::noul(group["instructions"].as_str().unwrap().replace("{item}", it["text"].as_str().unwrap()))))
                .collect();
            let dec = d.ask(group["state"].clone(), qs).await.unwrap();
            for (i, it) in items.iter().enumerate() {
                let p = dec.answers[&format!("q{i}")].yes().unwrap();
                let label = if it["label"].as_bool().unwrap() { 1.0 } else { 0.0 };
                total += (p - label).powi(2);
                let right = (p >= 0.5) == (label > 0.5);
                if !right {
                    println!("  miss: {} → {p:.2} (label {label})", it["text"]);
                }
                correct += right as usize;
                n += 1;
            }
            println!("{}: {} ms, {} tokens", group["name"], dec.elapsed.as_millis(), dec.input_tokens);
        }
        println!("{}: accuracy {}/{n}, brier {:.3}", d.model(), correct, total / n as f64);
    }
}
