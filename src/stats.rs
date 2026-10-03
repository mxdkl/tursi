//! Append-only JSONL cost ledgers, partitioned by month, and budget checks (§9).

use anyhow::Result;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use uuid::Uuid;

use crate::api::Usage;
use crate::bus::AgentId;
use crate::config::{Budgets, Price};

#[derive(Serialize, Deserialize)]
pub struct Entry {
    pub ts: DateTime<Utc>,
    pub session: Uuid,
    /// Root agent until subagents exist (§5.7); cost rollup is then a query,
    /// not a migration.
    pub agent: AgentId,
    pub task: u32,
    pub model: String,
    pub input_tokens: u64,
    pub cached_tokens: u64,
    pub output_tokens: u64,
    pub cost_usd: f64,
}

pub struct Ledger {
    dir: PathBuf,
}

#[derive(Debug, Clone, Copy, Default)]
pub struct SessionStats {
    pub cost_usd: f64,
    pub input_tokens: u64,
    pub cached_tokens: u64,
    pub output_tokens: u64,
    pub model_calls: u64,
}

impl Ledger {
    /// `~/.tursi/stats/`, created on first run.
    pub fn open() -> Result<Ledger> {
        Self::open_at(crate::config::Config::home_dir()?.join("stats"))
    }

    /// Explicit directory — tests, and anyone relocating their ledgers.
    pub fn open_at(dir: PathBuf) -> Result<Ledger> {
        std::fs::create_dir_all(&dir)?;
        Ok(Ledger { dir })
    }

    pub fn record(&self, entry: Entry) -> Result<()> {
        use std::io::Write;
        let path = self.dir.join(format!("{}.jsonl", entry.ts.format("%Y-%m")));
        let mut file = std::fs::OpenOptions::new().create(true).append(true).open(path)?;
        writeln!(file, "{}", serde_json::to_string(&entry)?)?;
        Ok(())
    }

    /// Status-bar figures are computed from the ledgers, never cached elsewhere.
    pub fn session_total(&self, session: Uuid) -> Result<f64> {
        self.sum(|e| e.session == session)
    }

    pub fn month_total(&self) -> Result<f64> {
        let month = Utc::now().format("%Y-%m").to_string();
        self.sum(|e| e.ts.format("%Y-%m").to_string() == month)
    }

    /// Aggregate a session's ledger entries for reporting (headless/bench):
    /// (cost, input, cached, output, model-calls).
    pub fn session_stats(&self, session: Uuid) -> Result<SessionStats> {
        let mut s = SessionStats::default();
        for file in std::fs::read_dir(&self.dir)? {
            let path = file?.path();
            if path.extension().and_then(|e| e.to_str()) != Some("jsonl") {
                continue;
            }
            for line in std::fs::read_to_string(&path)?.lines() {
                if let Ok(e) = serde_json::from_str::<Entry>(line) {
                    if e.session == session {
                        s.cost_usd += e.cost_usd;
                        s.input_tokens += e.input_tokens;
                        s.cached_tokens += e.cached_tokens;
                        s.output_tokens += e.output_tokens;
                        s.model_calls += 1;
                    }
                }
            }
        }
        Ok(s)
    }

    /// Tolerant scan: a corrupt line loses one entry, never the ledger.
    fn sum(&self, keep: impl Fn(&Entry) -> bool) -> Result<f64> {
        let mut total = 0.0;
        for file in std::fs::read_dir(&self.dir)? {
            let path = file?.path();
            if path.extension().and_then(|e| e.to_str()) != Some("jsonl") {
                continue;
            }
            for line in std::fs::read_to_string(&path)?.lines() {
                if let Ok(entry) = serde_json::from_str::<Entry>(line) {
                    if keep(&entry) {
                        total += entry.cost_usd;
                    }
                }
            }
        }
        Ok(total)
    }
}

pub fn cost(price: Price, usage: Usage) -> f64 {
    (price.input * usage.input_tokens as f64
        + price.cached_input * usage.cached_tokens as f64
        + price.output * usage.output_tokens as f64)
        / 1_000_000.0
}

pub enum BudgetStatus {
    Ok,
    /// Past 80% of a cap: status-bar warning.
    Warn(String),
    /// Past 100%: halt until explicitly confirmed (§9).
    Exceeded(String),
}

pub fn check(budgets: Budgets, session_usd: f64, month_usd: f64) -> BudgetStatus {
    let probe = |cap: Option<f64>, spent: f64, scope: &str| -> Option<BudgetStatus> {
        let cap = cap?;
        if spent >= cap {
            Some(BudgetStatus::Exceeded(format!("{scope} budget: ${spent:.2} of ${cap:.2}")))
        } else if spent >= cap * 0.8 {
            Some(BudgetStatus::Warn(format!("{scope} budget: ${spent:.2} of ${cap:.2}")))
        } else {
            None
        }
    };
    let session = probe(budgets.session_usd, session_usd, "session");
    let month = probe(budgets.month_usd, month_usd, "month");
    match (session, month) {
        (Some(BudgetStatus::Exceeded(m)), _) | (_, Some(BudgetStatus::Exceeded(m))) => {
            BudgetStatus::Exceeded(m)
        }
        (Some(w), _) | (_, Some(w)) => w,
        (None, None) => BudgetStatus::Ok,
    }
}
