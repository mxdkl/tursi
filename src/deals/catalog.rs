//! The station catalog (§5.8): which provider models can serve as DEALS
//! stations. `tursi --stations probe` reads Cloudflare's model list, keeps
//! the tool-calling text models with a large enough context and a listed
//! price, and runs each through a two-turn tool-use check. The results land
//! in `~/.tursi/deals/stations.json`; `tursi --stations` prints them with
//! what each station has learned.

use anyhow::{Context, Result, bail};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::path::PathBuf;
use std::time::{Duration, Instant};
use tokio::sync::mpsc;

use crate::api::{ChatRequest, Message, Provider, ToolSchema};
use crate::config::{Config, ModelOptions, Price, Secrets};

/// Subagent transcripts plus tool schemas need room: smaller contexts fail
/// mid-task.
pub const MIN_CONTEXT: u64 = 128_000;
/// Rough tokens per subagent task, for a price-derived cost guess before any
/// real costs are known: input re-sent every turn (mostly cached), plus
/// output.
const GUESS_INPUT: f64 = 60_000.0;
const GUESS_CACHED: f64 = 240_000.0;
const GUESS_OUTPUT: f64 = 6_000.0;

#[derive(Clone, Copy, Debug, Default, Serialize, Deserialize, PartialEq)]
pub struct Prices {
    pub input: f64,
    pub output: f64,
    pub cached_input: Option<f64>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Probe {
    pub ok: bool,
    pub ts: DateTime<Utc>,
    pub secs: f64,
    pub note: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Station {
    /// Full id with the provider prefix: `cloudflare/@cf/zai-org/glm-5.3-flash`.
    pub model: String,
    pub context: u64,
    pub prices: Prices,
    /// On the provider's paid tier (Cloudflare: 50 requests/min per model).
    pub paid: bool,
    pub reasoning: bool,
    /// The provider's capability tags that are set: `function_calling`,
    /// `vision`, `reasoning`, `async_queue`, `lora`, … (Cloudflare's catalog).
    #[serde(default)]
    pub caps: Vec<String>,
    pub probe: Option<Probe>,
}

/// Contexts at least this large count as `long_context`.
pub const LONG_CONTEXT: u64 = 500_000;

/// What a task can require of the model that runs it.
pub const NEEDS: [&str; 3] = ["vision", "reasoning", "long_context"];

impl Station {
    /// Whether this station has everything a task needs (§5.8): the
    /// capability tags are the paper's "eligible neighbors" filter.
    pub fn can(&self, needs: &[String]) -> bool {
        needs.iter().all(|n| match n.as_str() {
            "long_context" => self.context >= LONG_CONTEXT,
            tag => self.caps.iter().any(|c| c == tag),
        })
    }

    pub fn qualified(&self) -> bool {
        self.probe.as_ref().is_some_and(|p| p.ok)
    }

    pub fn price(&self) -> Price {
        Price { input: self.prices.input, output: self.prices.output, cached_input: self.prices.cached_input.unwrap_or(self.prices.input) }
    }

    /// Dollars a typical subagent task would cost here, before any real
    /// outcome is known.
    pub fn cost_guess(&self) -> f64 {
        let cached = self.prices.cached_input.unwrap_or(self.prices.input);
        (GUESS_INPUT * self.prices.input + GUESS_CACHED * cached + GUESS_OUTPUT * self.prices.output) / 1e6
    }

    /// Request options for a model the user has not configured: low effort
    /// on reasoning models (thinking bills as output), and an explicit
    /// output cap (some models default to 256 tokens, which truncates edits).
    pub fn default_options(&self) -> ModelOptions {
        ModelOptions { reasoning_effort: self.reasoning.then(|| "low".to_string()), max_tokens: Some(8192) }
    }
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Catalog {
    pub fetched: Option<DateTime<Utc>>,
    pub stations: Vec<Station>,
}

impl Catalog {
    pub fn path() -> Option<PathBuf> {
        let dir = Config::home_dir().ok()?.join("deals");
        std::fs::create_dir_all(&dir).ok()?;
        Some(dir.join("stations.json"))
    }

    pub fn load() -> Catalog {
        Self::path().and_then(|p| std::fs::read_to_string(p).ok()).and_then(|t| serde_json::from_str(&t).ok()).unwrap_or_default()
    }

    pub fn save(&self) -> Result<()> {
        let path = Self::path().context("no ~/.tursi")?;
        std::fs::write(path, serde_json::to_string_pretty(self)?)?;
        Ok(())
    }

    pub fn get(&self, model: &str) -> Option<&Station> {
        self.stations.iter().find(|s| s.model == model)
    }

    /// The pool: `[deals] stations` when given (catalog entries or bare
    /// configured models), otherwise every qualified model; minus `exclude`.
    pub fn pool(&self, config: &Config) -> Vec<Station> {
        let mut out: Vec<Station> = if config.deals.stations.is_empty() {
            self.stations.iter().filter(|s| s.qualified()).cloned().collect()
        } else {
            config
                .deals
                .stations
                .iter()
                .map(|m| {
                    self.get(m).cloned().unwrap_or_else(|| {
                        let p = config.prices.get(m).copied();
                        Station {
                            model: m.clone(),
                            context: MIN_CONTEXT,
                            prices: p.map(|p| Prices { input: p.input, output: p.output, cached_input: Some(p.cached_input) }).unwrap_or_default(),
                            paid: false,
                            reasoning: false,
                            caps: vec!["function_calling".into()],
                            probe: None,
                        }
                    })
                })
                .collect()
        };
        out.retain(|s| !config.deals.exclude.contains(&s.model));
        out
    }
}

/// `…/accounts/<id>/ai/v1` → `…/accounts/<id>/ai/models/search`.
fn search_url(base: &str) -> Option<String> {
    let root = base.trim_end_matches('/').strip_suffix("/ai/v1")?;
    Some(format!("{root}/ai/models/search"))
}

fn property<'a>(model: &'a Value, id: &str) -> Option<&'a Value> {
    model.get("properties")?.as_array()?.iter().find(|p| p.get("property_id").and_then(Value::as_str) == Some(id))?.get("value")
}

fn truthy(v: Option<&Value>) -> bool {
    match v {
        Some(Value::Bool(b)) => *b,
        Some(Value::String(s)) => s == "true",
        _ => false,
    }
}

/// One catalog entry, if the model can serve as a station at all.
fn parse_model(m: &Value) -> Option<Station> {
    let name = m.get("name")?.as_str()?;
    if !truthy(property(m, "function_calling")) || truthy(property(m, "beta")) {
        return None;
    }
    let context = property(m, "context_window").and_then(|v| v.as_u64().or_else(|| v.as_str()?.parse().ok()))?;
    if context < MIN_CONTEXT {
        return None;
    }
    let mut prices = Prices::default();
    for p in property(m, "price")?.as_array()? {
        let unit = p.get("unit").and_then(Value::as_str).unwrap_or("");
        let price = p.get("price").and_then(Value::as_f64).unwrap_or(0.0);
        if unit.contains("cached") {
            prices.cached_input = Some(price);
        } else if unit.contains("input") {
            prices.input = price;
        } else if unit.contains("output") {
            prices.output = price;
        }
    }
    if prices.input <= 0.0 || prices.output <= 0.0 {
        return None;
    }
    let caps = m
        .get("properties")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter(|p| truthy(p.get("value")))
        .filter_map(|p| p.get("property_id").and_then(Value::as_str))
        .filter(|id| !matches!(*id, "beta" | "require_workers_paid"))
        .map(String::from)
        .collect();
    Some(Station {
        model: format!("cloudflare/{name}"),
        context,
        prices,
        paid: truthy(property(m, "require_workers_paid")),
        reasoning: truthy(property(m, "reasoning")),
        caps,
        probe: None,
    })
}

/// Cloudflare's text-generation models that could be stations.
pub async fn fetch(secrets: &Secrets) -> Result<Vec<Station>> {
    let (token, base) = secrets.provider_for("cloudflare/x").context("no Cloudflare credentials in secrets.toml")?;
    let url = search_url(&base).context("the Cloudflare base URL is not an …/ai/v1 account URL")?;
    let http = reqwest::Client::builder().timeout(Duration::from_secs(30)).build()?;
    let mut out = Vec::new();
    for page in 1..=10 {
        let body: Value = http
            .get(format!("{url}?task=Text%20Generation&per_page=50&page={page}"))
            .bearer_auth(&token)
            .header("User-Agent", "tursi/0.1")
            .send()
            .await?
            .json()
            .await?;
        let models = body.get("result").and_then(Value::as_array).cloned().unwrap_or_default();
        out.extend(models.iter().filter_map(parse_model));
        if models.len() < 50 {
            break;
        }
    }
    if out.is_empty() {
        bail!("Cloudflare listed no tool-calling text models");
    }
    Ok(out)
}

/// Two turns: the model must call a tool, then use its result.
pub async fn probe(station: &Station, secrets: &Secrets, config: &Config) -> Probe {
    let started = Instant::now();
    let result = tokio::time::timeout(Duration::from_secs(150), probe_inner(station, secrets, config)).await;
    let (ok, note) = match result {
        Ok(Ok(())) => (true, "tool call and follow-up ok".to_string()),
        Ok(Err(e)) => (false, crate::output::redact(&format!("{e:#}")).chars().take(160).collect()),
        Err(_) => (false, "timed out".to_string()),
    };
    Probe { ok, ts: Utc::now(), secs: started.elapsed().as_secs_f64(), note }
}

async fn probe_inner(station: &Station, secrets: &Secrets, config: &Config) -> Result<()> {
    let mut config = config.clone();
    config.models.entry(station.model.clone()).or_insert_with(|| station.default_options());
    let provider = Provider::for_model(&station.model, secrets, &config)?;
    let tools = vec![ToolSchema {
        name: "read".into(),
        description: "Read a file and return its contents.".into(),
        parameters: json!({"type":"object","properties":{"file":{"type":"string"}},"required":["file"]}),
    }];
    let mut messages = vec![
        Message::System("You are being checked for tool use. Follow the user's instruction exactly.".into()),
        Message::User("Call the read tool on the file probe.txt, then reply with the single word that file contains and nothing else.".into()),
    ];
    let chat = |messages: Vec<Message>| {
        let (provider, tools, model) = (&provider, tools.clone(), station.model.clone());
        async move {
            crate::ratelimit::acquire(&model).await;
            let (tx, mut rx) = mpsc::channel(64);
            let drain = tokio::spawn(async move { while rx.recv().await.is_some() {} });
            let turn = provider.chat(ChatRequest { model, messages, tools }, tx).await;
            let _ = drain.await;
            turn
        }
    };
    let first = chat(messages.clone()).await?;
    let call = first.tool_calls.first().context(format!("no tool call (said: {:?})", first.text.chars().take(80).collect::<String>()))?;
    if call.name != "read" {
        bail!("called {:?} instead of read", call.name);
    }
    messages.push(Message::Assistant { text: first.text.clone(), tool_calls: vec![call.clone()] });
    messages.push(Message::ToolResult { call_id: call.id.clone(), content: "pelican".into(), is_error: false });
    let second = chat(messages).await?;
    if !second.text.to_lowercase().contains("pelican") {
        bail!("did not use the tool result (said: {:?})", second.text.chars().take(80).collect::<String>());
    }
    Ok(())
}

/// `tursi --stations [probe]`.
pub async fn run_cli(config: &Config, secrets: &Secrets, probe_all: bool) -> Result<()> {
    let mut catalog = Catalog::load();
    if probe_all || catalog.stations.is_empty() {
        let fresh = fetch(secrets).await?;
        println!("probing {} tool-calling models with ≥{}k context…", fresh.len(), MIN_CONTEXT / 1000);
        let mut probed = Vec::new();
        let mut set = tokio::task::JoinSet::new();
        for s in fresh {
            let (secrets, config) = (secrets.clone(), config.clone());
            set.spawn(async move {
                let p = probe(&s, &secrets, &config).await;
                Station { probe: Some(p), ..s }
            });
        }
        while let Some(done) = set.join_next().await {
            if let Ok(s) = done {
                let p = s.probe.as_ref().unwrap();
                println!("  {} {:<36} {:>5.1}s  {}", if p.ok { "✓" } else { "✗" }, super::short_model(&s.model), p.secs, p.note);
                probed.push(s);
            }
        }
        probed.sort_by(|a, b| a.model.cmp(&b.model));
        catalog = Catalog { fetched: Some(Utc::now()), stations: probed };
        catalog.save()?;
    }
    let expertise = super::expertise::Expertise::load(super::expertise::Expertise::default_path());
    let pool = catalog.pool(config);
    println!("\nlearned: predicted success at easy/moderate/hard (outcomes), then +/- activities and domains it does better/worse at");
    println!(
        "DEALS {} — {} stations (cheapest within {} of the best, {} slots each)",
        if config.deals.enabled { "on" } else { "off ([deals] enabled = false)" },
        pool.len(),
        config.deals.tolerance,
        config.deals.slots
    );
    for s in &catalog.stations {
        let in_pool = pool.iter().any(|p| p.model == s.model);
        // Overall chance at easy/moderate/hard, then the activities and
        // domains it does clearly better or worse at.
        let learned: Vec<String> = match expertise.facets(&s.model) {
            None => vec![],
            Some((_, facets)) => {
                let n = expertise.outcomes(&s.model);
                let p = |d: f64| expertise.p(&s.model, &super::Labels { difficulty: Some(d), ..Default::default() });
                let mut out = vec![format!("{:.2}/{:.2}/{:.2} ({n})", p(1.0), p(2.0), p(3.0))];
                out.extend(facets.iter().filter(|(_, x, _)| x.abs() >= 0.25).map(|(name, x, _)| format!("{}{name}", if *x > 0.0 { "+" } else { "-" })));
                vec![out.join(" ")]
            }
        };
        let tags: Vec<&str> = s
            .caps
            .iter()
            .filter_map(|c| match c.as_str() {
                "vision" => Some("vis"),
                "reasoning" => Some("rsn"),
                "async_queue" => Some("batch"),
                _ => None,
            })
            .collect();
        println!(
            "{} {:<36} {:>5}k  ${:<6} ${:<5} {:<4} {:<13} {}{}",
            if in_pool { "●" } else if s.qualified() { "○" } else { "✗" },
            super::short_model(&s.model),
            s.context / 1000,
            s.prices.input,
            s.prices.output,
            if s.paid { "paid" } else { "" },
            tags.join(","),
            if learned.is_empty() { "no outcomes yet".to_string() } else { learned.join(", ") },
            s.probe.as_ref().filter(|p| !p.ok).map(|p| format!("  [{}]", p.note)).unwrap_or_default()
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn catalog_entries_parse_into_stations() {
        let m = json!({"name": "@cf/zai-org/glm-5.3-flash", "properties": [
            {"property_id": "function_calling", "value": "true"},
            {"property_id": "context_window", "value": "1048576"},
            {"property_id": "require_workers_paid", "value": "true"},
            {"property_id": "reasoning", "value": "true"},
            {"property_id": "price", "value": [
                {"unit": "per M input tokens", "price": 0.15},
                {"unit": "per M output tokens", "price": 0.5},
                {"unit": "per M cached input tokens", "price": 0.03}]}]});
        let s = parse_model(&m).unwrap();
        assert_eq!(s.model, "cloudflare/@cf/zai-org/glm-5.3-flash");
        assert_eq!(s.prices, Prices { input: 0.15, output: 0.5, cached_input: Some(0.03) });
        assert!(s.paid && s.reasoning && s.context == 1_048_576);
        assert_eq!(s.caps, vec!["function_calling", "reasoning"]);
        assert!(s.can(&["reasoning".into(), "long_context".into()]) && !s.can(&["vision".into()]));
        assert_eq!(s.default_options().reasoning_effort.as_deref(), Some("low"));
        assert!(s.cost_guess() > 0.0);

        let small = json!({"name": "@cf/x/tiny", "properties": [
            {"property_id": "function_calling", "value": true},
            {"property_id": "context_window", "value": 32000},
            {"property_id": "price", "value": [{"unit": "per M input tokens", "price": 0.1}, {"unit": "per M output tokens", "price": 0.1}]}]});
        assert!(parse_model(&small).is_none(), "context too small");
        let no_tools = json!({"name": "@cf/x/chat", "properties": [{"property_id": "context_window", "value": 200000}]});
        assert!(parse_model(&no_tools).is_none());
    }

    #[test]
    fn search_url_derives_from_the_account_base() {
        assert_eq!(
            search_url("https://api.cloudflare.com/client/v4/accounts/abc/ai/v1").as_deref(),
            Some("https://api.cloudflare.com/client/v4/accounts/abc/ai/models/search")
        );
        assert!(search_url("https://api.deepseek.com/v1").is_none());
    }
}
