//! Each station's private memory of successful trajectories (§5.8, paper
//! §3.3 and Eqs. 4–5), per project: `.tursi/deals/memory/<model>.jsonl`.
//! A task about to run at a station retrieves up to k similar past
//! successes there as demonstrations. Relevance is cosine similarity of the
//! briefs' embeddings, nudged toward faster runs:
//! s = cos · (1 + 0.2 / (1 + minutes)), kept when s ≥ θ.

use anyhow::{Context, Result, bail};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::path::{Path, PathBuf};
use std::time::Duration;

use super::Labels;

/// One successful run: what was asked, the tool steps taken, the report.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Trajectory {
    pub ts: DateTime<Utc>,
    #[serde(flatten)]
    pub labels: Labels,
    pub brief: String,
    pub steps: String,
    pub report: String,
    pub secs: u64,
    pub embedding: Vec<f32>,
}

/// Characters of a brief that go into its embedding; bge reads 512 tokens.
const EMBED_CHARS: usize = 1800;
/// Characters of each part of a demonstration shown to the model.
const DEMO_BRIEF: usize = 600;
const DEMO_STEPS: usize = 1200;
const DEMO_REPORT: usize = 600;

pub struct Memory {
    dir: PathBuf,
    cap: usize,
}

fn slug(model: &str) -> String {
    super::short_model(model).chars().map(|c| if c.is_ascii_alphanumeric() || c == '.' || c == '-' { c } else { '-' }).collect()
}

fn clip(s: &str, n: usize) -> String {
    if s.chars().count() <= n {
        return s.to_string();
    }
    let mut out: String = s.chars().take(n).collect();
    out.push('…');
    out
}

impl Memory {
    pub fn new(project: &Path, cap: usize) -> Memory {
        Memory { dir: project.join(".tursi/deals/memory"), cap }
    }

    fn path(&self, model: &str) -> PathBuf {
        self.dir.join(format!("{}.jsonl", slug(model)))
    }

    pub fn load(&self, model: &str) -> Vec<Trajectory> {
        std::fs::read_to_string(self.path(model))
            .map(|t| t.lines().filter_map(|l| serde_json::from_str(l).ok()).collect())
            .unwrap_or_default()
    }

    /// Keep the newest `cap` trajectories (the paper's pool of 40).
    pub fn add(&self, model: &str, t: Trajectory) -> Result<()> {
        std::fs::create_dir_all(&self.dir)?;
        let mut all = self.load(model);
        all.push(t);
        if all.len() > self.cap {
            all.drain(..all.len() - self.cap);
        }
        let body: String = all.iter().filter_map(|t| serde_json::to_string(t).ok()).map(|l| l + "\n").collect();
        let tmp = self.path(model).with_extension("jsonl.tmp");
        std::fs::write(&tmp, body)?;
        std::fs::rename(tmp, self.path(model))?;
        Ok(())
    }
}

fn cosine(a: &[f32], b: &[f32]) -> f64 {
    if a.len() != b.len() || a.is_empty() {
        return 0.0;
    }
    let (mut dot, mut na, mut nb) = (0f64, 0f64, 0f64);
    for (x, y) in a.iter().zip(b) {
        dot += (*x as f64) * (*y as f64);
        na += (*x as f64).powi(2);
        nb += (*y as f64).powi(2);
    }
    if na == 0.0 || nb == 0.0 { 0.0 } else { dot / (na.sqrt() * nb.sqrt()) }
}

/// Paper Eq. 4.
pub fn relevance(query: &[f32], t: &Trajectory) -> f64 {
    cosine(query, &t.embedding) * (1.0 + 0.2 / (1.0 + t.secs as f64 / 60.0))
}

/// Paper Eq. 5: the top k at or above θ, best first.
pub fn retrieve<'a>(query: &[f32], pool: &'a [Trajectory], k: usize, theta: f64) -> Vec<&'a Trajectory> {
    let mut scored: Vec<(f64, &Trajectory)> = pool.iter().map(|t| (relevance(query, t), t)).filter(|(s, _)| *s >= theta).collect();
    scored.sort_by(|a, b| b.0.partial_cmp(&a.0).unwrap_or(std::cmp::Ordering::Equal));
    scored.into_iter().take(k).map(|(_, t)| t).collect()
}

/// The demonstration block that precedes a brief.
pub fn render(demos: &[&Trajectory]) -> String {
    if demos.is_empty() {
        return String::new();
    }
    let mut out = String::from(
        "[memory] Similar tasks you completed successfully in this project. Use them as a guide to where things \
         are and what worked, not as a script — the current brief is what counts.\n",
    );
    for (i, t) in demos.iter().enumerate() {
        out.push_str(&format!(
            "\n### Example {} ({}, {}s)\nBrief: {}\nSteps:\n{}\nReport: {}\n",
            i + 1,
            t.labels.name(),
            t.secs,
            clip(&t.brief, DEMO_BRIEF),
            clip(&t.steps, DEMO_STEPS),
            clip(&t.report, DEMO_REPORT)
        ));
    }
    out.push_str("\n[task]\n");
    out
}

/// Text embeddings from the provider's model-run endpoint (Cloudflare
/// Workers AI: `…/ai/run/@cf/baai/bge-large-en-v1.5`).
pub struct Embedder {
    http: reqwest::Client,
    token: String,
    url: String,
    headers: Vec<(String, String)>,
}

impl Embedder {
    pub fn from_config(config: &crate::config::Config, secrets: &crate::config::Secrets) -> Option<Embedder> {
        let full = &config.deals.embed_model;
        let (token, base) = secrets.provider_for(full)?;
        let model = full.split_once('/').map(|(_, m)| m)?;
        let run = format!("{}/run", base.trim_end_matches('/').trim_end_matches("/v1"));
        Some(Embedder {
            http: reqwest::Client::builder().timeout(Duration::from_secs(20)).build().ok()?,
            token,
            url: format!("{run}/{model}"),
            headers: secrets.headers_for(full),
        })
    }

    pub async fn embed(&self, text: &str) -> Result<Vec<f32>> {
        let mut req = self.http.post(&self.url).bearer_auth(&self.token).header("User-Agent", "tursi/0.1");
        for (k, v) in &self.headers {
            req = req.header(k, v);
        }
        let resp = req.json(&json!({"text": [clip(text, EMBED_CHARS)]})).send().await.context("embedding request")?;
        let status = resp.status();
        let body: Value = resp.json().await.context("embedding reply")?;
        if !status.is_success() {
            bail!("embedding: HTTP {status}: {}", crate::output::redact(&body.to_string()).chars().take(200).collect::<String>());
        }
        let row = body.pointer("/result/data/0").and_then(Value::as_array).context("no embedding in reply")?;
        Ok(row.iter().filter_map(Value::as_f64).map(|x| x as f32).collect())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn traj(embedding: Vec<f32>, secs: u64) -> Trajectory {
        Trajectory { ts: Utc::now(), labels: Labels { activity: Some(super::super::Activity::New), domain: Some(super::super::Domain::Systems), difficulty: None }, brief: "b".into(), steps: "s".into(), report: "r".into(), secs, embedding }
    }

    #[test]
    fn retrieval_keeps_the_relevant_and_favors_fast_runs() {
        let q = vec![1.0, 0.0];
        let pool = vec![
            traj(vec![1.0, 0.0], 3600), // identical, slow: 1·(1+0.2/61)
            traj(vec![1.0, 0.05], 0),   // near-identical, instant: ~1.2
            traj(vec![0.0, 1.0], 0),    // unrelated
            traj(vec![0.6, 0.8], 0),    // cos 0.6·1.2 = 0.72
        ];
        let got = retrieve(&q, &pool, 3, 0.7);
        assert_eq!(got.len(), 3);
        assert_eq!(got[0].secs, 0);
        assert!(got.iter().all(|t| t.embedding != vec![0.0, 1.0]));
        assert_eq!(retrieve(&q, &pool, 1, 0.7).len(), 1);
        assert!(retrieve(&q, &pool, 3, 1.3).is_empty());
    }

    #[test]
    fn memory_keeps_the_newest_up_to_its_cap() {
        let dir = std::env::temp_dir().join(format!("tursi-deals-mem-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let mem = Memory::new(&dir, 2);
        for secs in [1, 2, 3] {
            mem.add("cloudflare/@cf/zai-org/glm-5.3-flash", traj(vec![1.0], secs)).unwrap();
        }
        let kept: Vec<u64> = mem.load("cloudflare/@cf/zai-org/glm-5.3-flash").iter().map(|t| t.secs).collect();
        assert_eq!(kept, vec![2, 3]);
        assert!(dir.join(".tursi/deals/memory/glm-5.3-flash.jsonl").exists());
        assert!(render(&[]).is_empty());
        assert!(render(&[&traj(vec![], 5)]).contains("### Example 1 (new·systems, 5s)"));
        let _ = std::fs::remove_dir_all(dir);
    }
}
