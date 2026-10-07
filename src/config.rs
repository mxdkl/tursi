//! Layered configuration (§1): defaults ← `~/.tursi/config.toml` ←
//! `<project>/.tursi/config.toml`. Secrets live in a separate 0600 file.

use anyhow::{Context, Result, bail};
use serde::Deserialize;
use std::collections::HashMap;
use std::path::{Path, PathBuf};

/// Provider base URLs used when secrets.toml doesn't override them. Provider
/// infrastructure, not model policy — models/prices stay pure config (§7.1).
const DEFAULT_BASE_URLS: &[(&str, &str)] = &[
    ("deepseek", "https://api.deepseek.com/v1"),
    ("openrouter", "https://openrouter.ai/api/v1"),
    ("kimi", "https://api.moonshot.ai/v1"),
    ("qwen", "https://dashscope.aliyuncs.com/compatible-mode/v1"),
];

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct Config {
    /// The model every task runs on. No model IDs in code — the first run
    /// seeds a starter config the user owns from then on.
    pub model: String,
    /// Tried in order when `model` keeps failing at the API level (retries
    /// exhausted). Same provider prefix only — the provider client is built
    /// once per task.
    pub fallbacks: Vec<String>,
    /// The removed `[[tiers]]` ladder, captured only so an old config fails
    /// with a migration hint instead of a bare "no model configured".
    #[serde(alias = "tier")]
    tiers: Option<toml::Value>,
    /// $/M-token prices per model id, for cost accounting (§9).
    pub prices: HashMap<String, Price>,
    /// Per-model request options: `[models."<id>"] reasoning_effort = "low"`.
    /// Reasoning models bill their thinking as output, so the effort setting
    /// is a cost knob, not a quality footnote.
    pub models: HashMap<String, ModelOptions>,
    /// The removed allow/deny command patterns (PERMISSIONS.md §7), captured
    /// only to warn: the sandbox replaced them.
    permissions: Option<toml::Value>,
    pub budgets: Budgets,
    /// Cap on per-step `timeout_seconds` (default 600).
    pub timeout_cap_seconds: u64,
    /// AFK safety valve (§5.2, default 40).
    pub max_turns_per_task: u32,
    /// AFK verification gate (§5.3), e.g. ["cargo check", "cargo test"].
    pub verify: Vec<String>,
    /// Plan-mode gate: typecheck only (§5.5), e.g. ["cargo check"].
    pub typecheck: Vec<String>,
    /// Extra sandbox mounts (PERMISSIONS.md §7). Global config only: a
    /// project's own config can't widen its sandbox.
    pub sandbox: SandboxConfig,
    /// Hosts reachable from the sandbox without asking, on top of the package
    /// registries (PERMISSIONS.md §4). `host`, `.suffix`, or `host:port`.
    pub network: NetworkConfig,
    /// Subagent settings (§5.7): `[subagent] model = "…"`. Without a model
    /// subagents run on the parent's; `max_turns` defaults to 30. With DEALS
    /// on, the model is where new tasks enter the pool.
    pub subagent: AgentConfig,
    /// `/goal` evaluation (§5.3): which model judges the condition (default
    /// the main one; a cheap model does fine) and the most goal turns before
    /// the loop stops on its own.
    pub goal: GoalConfig,
    /// Decision model (`decide` tool; shadow gate and routing): `[decide]
    /// model = "cloudflare/@cf/cloudflare/clef-flash"`.
    pub decide: DecideConfig,
    /// `--station`: one model alone with every tool, no subagents. Set at
    /// run time, never from a file.
    #[serde(skip)]
    pub solo: bool,
    /// Where the provider's prepaid balance is read from (§9). Derived for
    /// Cloudflare AI Gateway; `[balance] url = …` for anything else.
    pub balance: BalanceConfig,
    /// DEALS task serving (§5.8): subagent tasks queue at model stations and
    /// route by backlog, learned success rate, and cost.
    pub deals: DealsConfig,
    /// Request-rate limits per model and per gateway (§7.2).
    pub limits: LimitsConfig,
    /// Context fraction that triggers compaction (§8).
    pub compact_at: f32,
    /// The model's context window in tokens — the compaction denominator.
    pub context_window: u64,
    /// Compact early once the context passes this many tokens and old tool
    /// output is a large share of it (§8): every call re-bills the whole
    /// context, so a long task on a bloated transcript costs more than the
    /// one cache miss a compaction causes. 0 = only the window trigger.
    pub compact_min_tokens: u64,
    /// Per-language LSP command overrides (§8), e.g.
    /// `python = "uv run basedpyright-langserver --stdio"`. Top of the
    /// resolution ladder; project-env detection handles the rest.
    pub lsp: HashMap<String, String>,
    /// Run a language-server diagnostics pass after each edit/write, spawning
    /// the server on first touch (§3.1). Surfaces type/import errors — an
    /// undefined symbol, a type-only import used as a value — the moment they
    /// are written, without the agent opting in via code_intel. Degrades to no
    /// diagnostics when no server is installed. Default on.
    pub lsp_check_edits: bool,
}

impl Default for Config {
    fn default() -> Self {
        Config {
            model: String::new(),
            fallbacks: Vec::new(),
            tiers: None,
            prices: HashMap::new(),
            models: HashMap::new(),
            permissions: None,
            budgets: Budgets::default(),
            timeout_cap_seconds: 600,
            // 0 = no turn cap; budget caps and Esc are the real bounds (§9).
            max_turns_per_task: 0,
            verify: Vec::new(),
            typecheck: Vec::new(),
            sandbox: SandboxConfig::default(),
            network: NetworkConfig::default(),
            subagent: AgentConfig::default(),
            goal: GoalConfig::default(),
            decide: DecideConfig::default(),
            solo: false,
            balance: BalanceConfig::default(),
            deals: DealsConfig::default(),
            limits: LimitsConfig::default(),
            compact_at: 0.75,
            context_window: crate::agent::prompt::DEFAULT_CONTEXT_WINDOW,
            compact_min_tokens: 24_000,
            lsp: HashMap::new(),
            lsp_check_edits: true,
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct GoalConfig {
    pub model: Option<String>,
    pub max_turns: u32,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct BalanceConfig {
    pub url: Option<String>,
    /// JSON pointer to the balance in the reply.
    pub pointer: String,
    /// Reply units per dollar.
    pub divisor: f64,
}

impl Default for BalanceConfig {
    fn default() -> Self {
        BalanceConfig { url: None, pointer: "/result/balance".into(), divisor: 100.0 }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct DecideConfig {
    pub model: Option<String>,
    /// Make and log the gate/routing decisions without acting on them.
    pub shadow: bool,
}

impl Default for DecideConfig {
    fn default() -> Self {
        DecideConfig { model: None, shadow: true }
    }
}

/// DEALS (§5.8, arXiv 2609.33768). Defaults follow the paper's Table 3
/// except where noted: V sits between its homogeneous (20) and mixed (80)
/// settings, and success is modelled per difficulty (expertise.rs) rather
/// than as one Beta-smoothed rate per type.
#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct DealsConfig {
    /// Off: every subagent runs on the `[subagent]` model, immediately.
    pub enabled: bool,
    /// Station models. Empty: every model that passed `tursi --stations probe`.
    pub stations: Vec<String>,
    /// Models never used as stations, even when probed fine.
    pub exclude: Vec<String>,
    /// Routing is quality first: stations whose chance of success for a task
    /// is within this of the best one's count as able to do it, and the
    /// cheapest of them takes it.
    pub tolerance: f64,
    /// Hop limit H (paper: 3).
    pub hops: u32,
    /// Split limit K (paper: 3).
    pub splits: u32,
    /// Execution slots per station C (paper: 9); a model's rate budget can
    /// lower it (§7.2).
    pub slots: u32,
    /// Tasks executing across all stations at once.
    pub max_running: u32,
    /// Back pressure: past this many queued tasks, `agent` refuses new ones.
    pub max_queued: u32,
    /// Exploration guard (not in the paper): until a station has this many
    /// outcomes for a type, routing sends it at most one such task at a time,
    /// so an untried model learns on one task instead of a whole burst.
    pub probation: u32,
    /// Retrieved demonstrations per task (k), their relevance floor (θ), and
    /// how many successful trajectories a station keeps per project.
    pub memory_k: usize,
    pub memory_theta: f64,
    pub memory_cap: usize,
    /// Embedding model for trajectory retrieval.
    pub embed_model: String,
    /// Judge every finished task with the decision model (`[decide]`); off,
    /// the outcome kind alone sets the success estimate.
    pub qa: bool,
    /// Let a subagent `fork` independent parts of its task into parallel
    /// subtasks and continue when all finish. Not in the paper (DEALS only
    /// splits sequentially); an extension (§5.8).
    pub fork: bool,
    /// Most subtasks one fork may start.
    pub fork_width: u32,
    /// How deep forks nest: 1 = only tasks the lead filed may fork.
    pub fork_depth: u32,
    /// Training runs: route on a draw from each station's learned success
    /// instead of the estimate itself (Thompson sampling), so stations with
    /// few outcomes get tried, at the price of sometimes sending work to a
    /// dearer or weaker one. Off (default): the current best guess, which is
    /// what real work should get.
    pub explore: bool,
    /// Coverage for training runs: until every eligible station has this
    /// many outcomes, a new task goes to the least-tried one, whatever the
    /// scores say. 0 (default): off, exploration is Thompson sampling alone.
    pub explore_min: u32,
    /// Every user message goes straight into the pool as a task (§5.7): no
    /// model reads it first. Off: the root agent works on it itself, with
    /// every tool, and may file pool tasks with `agent`.
    pub pipeline: bool,
    /// Seconds a subagent segment may run on a moderate task; doubles per
    /// difficulty level above, halves per level below (1 to 30 minutes).
    /// Past it the segment ends like a turn limit and the task continues on
    /// another station. 0: no limit.
    pub segment_secs: u64,
}

impl Default for DealsConfig {
    fn default() -> Self {
        DealsConfig {
            enabled: false,
            stations: Vec::new(),
            exclude: Vec::new(),
            tolerance: 0.03,
            hops: 3,
            splits: 3,
            slots: 9,
            max_running: 24,
            max_queued: 64,
            probation: 3,
            memory_k: 3,
            memory_theta: 0.7,
            memory_cap: 40,
            embed_model: "cloudflare/@cf/baai/bge-large-en-v1.5".into(),
            qa: true,
            fork: true,
            fork_width: 8,
            fork_depth: 1,
            explore: false,
            explore_min: 0,
            pipeline: true,
            segment_secs: 300,
        }
    }
}

/// Requests per minute. Cloudflare caps every paid-access model at 50/min
/// per account on unified billing (20 on standard billing), others at 300,
/// and unified-billing traffic at 200/min per gateway.
#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct LimitsConfig {
    /// Per model id; models not listed use `default_rpm` (or the paid-tier
    /// rate when the station catalog marks them paid).
    pub rpm: HashMap<String, u32>,
    pub default_rpm: u32,
    pub paid_rpm: u32,
    /// All models together, per provider gateway. 0 = no shared cap.
    pub gateway_rpm: u32,
}

impl Default for LimitsConfig {
    fn default() -> Self {
        LimitsConfig { rpm: HashMap::new(), default_rpm: 300, paid_rpm: 50, gateway_rpm: 200 }
    }
}

impl Default for GoalConfig {
    fn default() -> Self {
        GoalConfig { model: None, max_turns: 50 }
    }
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct ModelOptions {
    /// Sent as `reasoning_effort`; values are the provider's (DeepSeek and
    /// Kimi accept "none", GLM "low", gpt-oss "low"/"medium"/"high").
    pub reasoning_effort: Option<String>,
    /// Sent as `max_tokens`. Most providers default to the model's limit;
    /// Cloudflare's gpt-oss models default to 256, which truncates edits.
    pub max_tokens: Option<u32>,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct AgentConfig {
    pub model: Option<String>,
    pub max_turns: Option<u32>,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct NetworkConfig {
    pub allow: Vec<String>,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct SandboxConfig {
    /// Extra read-only paths; a leading `~/` is the home directory.
    pub read_only: Vec<String>,
    /// Extra writable paths — use sparingly.
    pub read_write: Vec<String>,
}

impl SandboxConfig {
    pub fn expand(list: &[String]) -> Vec<PathBuf> {
        let home = std::env::var_os("HOME").map(PathBuf::from);
        list.iter()
            .map(|p| match (p.strip_prefix("~/"), &home) {
                (Some(rest), Some(home)) => home.join(rest),
                _ => PathBuf::from(p),
            })
            .collect()
    }
}

#[derive(Debug, Clone, Copy, Deserialize)]
pub struct Price {
    pub input: f64,
    pub output: f64,
    pub cached_input: f64,
}

#[derive(Debug, Clone, Copy, Default, Deserialize)]
pub struct Budgets {
    pub session_usd: Option<f64>,
    pub month_usd: Option<f64>,
}

/// The project file overrides only what a project plausibly owns (§1):
/// verify/typecheck commands and permission additions.
#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct ProjectOverrides {
    /// A project may pick its own model; `fallbacks` reset unless it sets
    /// them too (they must share the model's provider).
    model: Option<String>,
    fallbacks: Option<Vec<String>>,
    #[serde(default)]
    models: HashMap<String, ModelOptions>,
    /// The subagent model for this project (merged field by field).
    #[serde(default)]
    subagent: AgentConfig,
    verify: Option<Vec<String>>,
    typecheck: Option<Vec<String>>,
    #[serde(default)]
    lsp: HashMap<String, String>,
}

/// Keys a project config may set; anything else is a typo or a global-only
/// key, and silently ignoring it once cost a whole model evaluation.
const PROJECT_KEYS: &[&str] = &["model", "fallbacks", "models", "subagent", "verify", "typecheck", "lsp"];

/// Written once on first run (§7.1: model IDs live in the user's config,
/// never in code — this seed is theirs to edit). Must parse: the test holds
/// it to that.
const STARTER_CONFIG: &str = r#"# tursi configuration — created on first run; edit freely. SPEC.md documents it.
#
# API keys come from the environment (<PREFIX>_API_KEY, e.g. DEEPSEEK_API_KEY)
# or from ~/.tursi/secrets.toml (chmod 600):
#   [providers.deepseek]
#   key = "env:DEEPSEEK_API_KEY"
# A gateway or any other OpenAI-compatible endpoint is a provider with its
# own base_url and, if it needs them, extra request headers:
#   [providers.cloudflare]
#   key = "env:CLOUDFLARE_API_TOKEN"
#   base_url = "https://api.cloudflare.com/client/v4/accounts/<account_id>/ai/v1"
#   [providers.cloudflare.headers]
#   cf-aig-gateway-id = "tursi"

# The model every task runs on. Any OpenAI-compatible provider works by
# prefix: "kimi/<model>", "qwen/<model>", "openrouter/<vendor/model>".
model = "deepseek/deepseek-flash"        # DeepSeek-V4.1-Flash
# Tried in order when the model keeps failing at the API (same provider only):
# fallbacks = ["deepseek/deepseek-v4-pro"]
# context_window = 131072                # tokens; compaction triggers at 75%
# compact_min_tokens = 24000             # …or earlier, once old tool output bloats the context

# $/M tokens — DeepSeek peak rates (off-peak is ~half during their discount
# window; using peak so the cost meter never under-reports).
[prices."deepseek/deepseek-flash"]
input = 0.30          # cache miss
output = 1.20
cached_input = 0.006  # cache hit

# Reasoning models think before every reply and bill it as output. Set the
# effort per model (names are the provider's own):
# [models."cloudflare/@cf/zai-org/glm-5.3"]
# reasoning_effort = "low"

[prices."deepseek/deepseek-v4-pro"]
input = 1.32          # cache miss
output = 3.96
cached_input = 0.044  # cache hit

# Everything the agent runs is contained by the sandbox (PERMISSIONS.md), so
# there are no command allow/deny lists. The one thing that asks is network
# access beyond the package registries.

# Hosts the sandbox may reach without asking, besides the package registries
# (crates.io, npm, PyPI, Go). Anything else needs `network: "full"` on the
# step, which asks you each time. Forms: "host", ".suffix", "host:port".
# [network]
# allow = ["github.com"]

# Subagents run on the main model unless [subagent] names its own. A cheap
# model suits most delegated work. With DEALS on, the pool picks each task's
# model and this is only where tasks enter it.
# [subagent]
# model = "deepseek/deepseek-flash"
# max_turns = 30

# DEALS (SPEC §5.8): tasks queue at a pool of models and go to the cheapest
# one about as likely as the best to get them done. Find the pool with
# `tursi --stations probe`; it learns from graded outcomes. With it on, each
# message you type goes straight into the pool as a task (no model reads it
# first); `pipeline = false` has the main model work on it instead.
# [deals]
# enabled = true
# pipeline = false

# `/goal <condition>` keeps working until a separate evaluator says the
# condition holds. A cheap model is enough to judge evidence.
# [goal]
# model = "deepseek/deepseek-flash"
# max_turns = 50

# Per-project .tursi/config.toml can override:
#   model     = "…" (and fallbacks)             # this project runs on another model
#   [subagent] model = "…"                      # and its subagents too
#   verify    = ["cargo check", "cargo test"]   # AFK gate (SPEC §5.3)
#   typecheck = ["cargo check"]                 # plan gate (SPEC §5.5)
#   and [lsp] server overrides (SPEC §8)
"#;

impl Config {
    /// Defaults ← global config ← project config, in that order. A missing
    /// global config is first-run: the starter gets written, then loaded.
    pub fn load(project: &Path) -> Result<Config> {
        Self::load_with_home(&Self::home_dir()?, project)
    }

    pub(crate) fn load_with_home(home: &Path, project: &Path) -> Result<Config> {
        let global = home.join("config.toml");
        if !global.exists() {
            std::fs::write(&global, STARTER_CONFIG)
                .with_context(|| format!("writing starter config to {}", global.display()))?;
            tracing::info!("created starter config at {}", global.display());
        }
        let raw = std::fs::read_to_string(&global)
            .with_context(|| format!("reading {}", global.display()))?;
        let mut config: Config =
            toml::from_str(&raw).with_context(|| format!("parsing {}", global.display()))?;
        if config.model.is_empty()
            && let Some(tiers) = &config.tiers
        {
            let first = tiers
                .as_array()
                .and_then(|t| t.first())
                .and_then(|t| t.get("models"))
                .and_then(|m| m.as_array())
                .and_then(|m| m.first())
                .and_then(|m| m.as_str())
                .unwrap_or("<provider>/<model>");
            bail!(
                "{} uses the removed [[tiers]] ladder — replace it with `model = \"{first}\"` \
                 (optionally `fallbacks = [...]`)",
                global.display()
            );
        }
        let local = project.join(".tursi/config.toml");
        if local.exists() {
            let raw = std::fs::read_to_string(&local)?;
            let over: ProjectOverrides =
                toml::from_str(&raw).with_context(|| format!("parsing {}", local.display()))?;
            if let Ok(table) = toml::from_str::<toml::Table>(&raw) {
                for key in table.keys().filter(|k| !PROJECT_KEYS.contains(&k.as_str())) {
                    tracing::warn!("{}: `{key}` is not a per-project setting — ignored", local.display());
                }
            }
            if let Some(model) = over.model {
                config.model = model;
                config.fallbacks = over.fallbacks.unwrap_or_default();
            } else if let Some(fallbacks) = over.fallbacks {
                config.fallbacks = fallbacks;
            }
            config.models.extend(over.models);
            if over.subagent.model.is_some() {
                config.subagent.model = over.subagent.model;
            }
            if over.subagent.max_turns.is_some() {
                config.subagent.max_turns = over.subagent.max_turns;
            }
            if let Some(verify) = over.verify {
                config.verify = verify;
            }
            if let Some(typecheck) = over.typecheck {
                config.typecheck = typecheck;
            }
            config.lsp.extend(over.lsp);
        }
        if config.permissions.take().is_some() {
            tracing::warn!(
                "{}: [permissions] allow/deny patterns are no longer used — the sandbox replaced them \
                 (PERMISSIONS.md); remove the section",
                global.display()
            );
        }
        Ok(config)
    }

    /// `~/.tursi`, created on first run.
    pub fn home_dir() -> Result<PathBuf> {
        let home = std::env::var_os("HOME").context("HOME is not set")?;
        let dir = PathBuf::from(home).join(".tursi");
        std::fs::create_dir_all(&dir)?;
        Ok(dir)
    }
}

/// API keys and provider base URLs, split from config so `tar ~/.tursi`
/// is safe (§1). Values are literal or `env:VAR_NAME` indirection.
#[derive(Clone)]
pub struct Secrets {
    keys: HashMap<String, String>,
    base_urls: HashMap<String, String>,
    /// Extra request headers per provider (e.g. AI Gateway's
    /// `cf-aig-gateway-id`). Values may be `env:VAR`.
    headers: HashMap<String, Vec<(String, String)>>,
}

impl Secrets {
    /// Loads `~/.tursi/secrets.toml`; refuses permissions looser than 0600.
    /// A missing file is fine — env-var fallback still works.
    pub fn load() -> Result<Secrets> {
        Self::load_from(&Config::home_dir()?.join("secrets.toml"))
    }

    /// Two accepted shapes, because both are things people reach for:
    ///   [providers.deepseek]        # nested
    ///   key = "sk-…"
    ///   base_url = "…"              # optional; [providers.<name>.headers] too
    /// and, forgivingly, the flat env-style form:
    ///   DEEPSEEK_API_KEY = "sk-…"   # <PREFIX>_API_KEY → provider <prefix>
    /// Values may be literal or `env:VAR_NAME`.
    pub(crate) fn load_from(path: &Path) -> Result<Secrets> {
        let mut secrets = Secrets { keys: HashMap::new(), base_urls: HashMap::new(), headers: HashMap::new() };
        if !path.exists() {
            return Ok(secrets);
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(path)?.permissions().mode();
            if mode & 0o077 != 0 {
                bail!("{} is readable by others — chmod 600 it", path.display());
            }
        }
        let table: toml::Table = toml::from_str(&std::fs::read_to_string(path)?)
            .with_context(|| format!("parsing {}", path.display()))?;

        for (name, value) in &table {
            match value {
                // Nested: [providers.<name>] key/base_url/headers.
                toml::Value::Table(inner) if name == "providers" => {
                    for (provider, entry) in inner {
                        let Some(entry) = entry.as_table() else { continue };
                        if let Some(key) = entry.get("key").and_then(|k| k.as_str()).and_then(resolve) {
                            secrets.keys.insert(provider.clone(), key);
                        }
                        if let Some(url) = entry.get("base_url").and_then(|u| u.as_str()) {
                            secrets.base_urls.insert(provider.clone(), url.trim_end_matches('/').to_string());
                        }
                        if let Some(headers) = entry.get("headers").and_then(|h| h.as_table()) {
                            let list: Vec<(String, String)> = headers
                                .iter()
                                .filter_map(|(k, v)| v.as_str().and_then(resolve).map(|v| (k.clone(), v)))
                                .collect();
                            secrets.headers.insert(provider.clone(), list);
                        }
                    }
                }
                // Flat: DEEPSEEK_API_KEY = "…" → provider "deepseek". Nested
                // wins if both are present (insert only when absent).
                toml::Value::String(v) => {
                    if let Some(prefix) = name.strip_suffix("_API_KEY") {
                        if let Some(key) = resolve(v) {
                            secrets.keys.entry(prefix.to_ascii_lowercase()).or_insert(key);
                        }
                    }
                }
                _ => {}
            }
        }
        Ok(secrets)
    }

    /// (key, base_url) for the provider that serves `model` (prefix-routed:
    /// "deepseek/…" → deepseek). Falls back to `<PREFIX>_API_KEY` in the env
    /// and the built-in base-URL table.
    pub fn provider_for(&self, model: &str) -> Option<(String, String)> {
        let prefix = model.split('/').next()?;
        let key = self
            .keys
            .get(prefix)
            .cloned()
            .or_else(|| std::env::var(format!("{}_API_KEY", prefix.to_ascii_uppercase())).ok())?;
        let base = self
            .base_urls
            .get(prefix)
            .cloned()
            .or_else(|| {
                DEFAULT_BASE_URLS
                    .iter()
                    .find(|(p, _)| *p == prefix)
                    .map(|(_, url)| (*url).to_string())
            })?;
        Some((key, base))
    }

    /// Extra headers for the provider that serves `model` (none by default).
    pub fn headers_for(&self, model: &str) -> Vec<(String, String)> {
        model.split('/').next().and_then(|p| self.headers.get(p)).cloned().unwrap_or_default()
    }
}

/// `env:VAR` indirection; a literal passes through.
fn resolve(value: &str) -> Option<String> {
    match value.strip_prefix("env:") {
        Some(var) => std::env::var(var).ok(),
        None => Some(value.to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tools::testutil;

    #[test]
    fn secrets_accepts_both_nested_and_flat_shapes() {
        let dir = testutil::tmp("cfg-secrets");
        let nested = dir.join("nested.toml");
        std::fs::write(&nested, "[providers.deepseek]\nkey = \"literal-nested\"\n").unwrap();
        set_600(&nested);
        assert_eq!(
            Secrets::load_from(&nested).unwrap().provider_for("deepseek/deepseek-chat").unwrap().0,
            "literal-nested"
        );

        // The exact shape that tripped the user up: flat env-style.
        let flat = dir.join("flat.toml");
        std::fs::write(&flat, "DEEPSEEK_API_KEY = \"literal-flat\"\n").unwrap();
        set_600(&flat);
        let secrets = Secrets::load_from(&flat).unwrap();
        let (key, base) = secrets.provider_for("deepseek/deepseek-chat").unwrap();
        assert_eq!(key, "literal-flat");
        assert!(base.contains("deepseek.com"), "default base URL resolves: {base}");

        // env: indirection resolves in the flat shape too.
        let env = dir.join("env.toml");
        std::fs::write(&env, "KIMI_API_KEY = \"env:TURSI_TEST_KIMI\"\n").unwrap();
        set_600(&env);
        unsafe { std::env::set_var("TURSI_TEST_KIMI", "from-env") };
        assert_eq!(
            Secrets::load_from(&env).unwrap().provider_for("kimi/k2").unwrap().0,
            "from-env"
        );

        // A gateway provider: custom base URL plus headers on every request,
        // and the model keeps everything after the provider prefix.
        let gw = dir.join("gateway.toml");
        std::fs::write(
            &gw,
            "[providers.cloudflare]\nkey = \"cf-token\"\nbase_url = \"https://api.cloudflare.com/client/v4/accounts/abc/ai/v1/\"\n\
             [providers.cloudflare.headers]\ncf-aig-gateway-id = \"tursi\"\ncf-aig-metadata = \"env:TURSI_TEST_KIMI\"\n",
        )
        .unwrap();
        set_600(&gw);
        let secrets = Secrets::load_from(&gw).unwrap();
        let (key, base) = secrets.provider_for("cloudflare/deepseek/deepseek-v4-pro").unwrap();
        assert_eq!((key.as_str(), base.as_str()), ("cf-token", "https://api.cloudflare.com/client/v4/accounts/abc/ai/v1"));
        let mut headers = secrets.headers_for("cloudflare/deepseek/deepseek-v4-pro");
        headers.sort();
        assert_eq!(headers, vec![("cf-aig-gateway-id".into(), "tursi".into()), ("cf-aig-metadata".into(), "from-env".into())]);
        assert!(secrets.headers_for("deepseek/deepseek-chat").is_empty());
    }

    #[cfg(unix)]
    fn set_600(path: &std::path::Path) {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600)).unwrap();
    }
    #[cfg(not(unix))]
    fn set_600(_: &std::path::Path) {}

    #[test]
    fn a_project_config_can_pick_its_own_model() {
        let home = testutil::tmp("cfg-home-pm");
        let project = testutil::tmp("cfg-proj-pm");
        std::fs::create_dir_all(project.join(".tursi")).unwrap();
        std::fs::write(
            project.join(".tursi/config.toml"),
            "model = \"cloudflare/@cf/zai-org/glm-5.3-flash\"\n[models.\"cloudflare/@cf/zai-org/glm-5.3-flash\"]\nreasoning_effort = \"low\"\n",
        )
        .unwrap();
        let config = Config::load_with_home(&home, &project).unwrap();
        assert_eq!(config.model, "cloudflare/@cf/zai-org/glm-5.3-flash");
        assert!(config.fallbacks.is_empty(), "global fallbacks don't follow a project's model");
        assert_eq!(config.models[&config.model].reasoning_effort.as_deref(), Some("low"));

        std::fs::write(project.join(".tursi/config.toml"), "[subagent]\nmodel = \"cloudflare/@cf/moonshotai/kimi-k2.6\"\n").unwrap();
        let config = Config::load_with_home(&home, &project).unwrap();
        assert_eq!(config.subagent.model.as_deref(), Some("cloudflare/@cf/moonshotai/kimi-k2.6"));
    }

    #[test]
    fn first_run_seeds_a_starter_config_that_actually_works() {
        let home = testutil::tmp("cfg-home");
        let project = testutil::tmp("cfg-proj");
        let config = Config::load_with_home(&home, &project).unwrap();
        assert!(home.join("config.toml").exists());
        assert!(!config.model.is_empty(), "starter must name a model");
        assert!(config.prices.contains_key(&config.model), "starter prices its model");
        assert!(config.network.allow.is_empty(), "no extra hosts by default");
        // Idempotent: the seeded file reloads, never gets rewritten.
        let again = Config::load_with_home(&home, &project).unwrap();
        assert_eq!(again.model, config.model);
    }

    #[test]
    fn a_legacy_tiers_config_fails_with_a_migration_hint() {
        let home = testutil::tmp("cfg-legacy");
        let project = testutil::tmp("cfg-legacy-proj");
        std::fs::write(
            home.join("config.toml"),
            "[[tiers]]\nname = \"workhorse\"\nmodels = [\"deepseek/deepseek-flash\"]\n",
        )
        .unwrap();
        let err = Config::load_with_home(&home, &project).unwrap_err().to_string();
        assert!(err.contains("model = \"deepseek/deepseek-flash\""), "{err}");
    }
}
