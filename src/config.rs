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
    /// Per-role subagent settings (§5.7): `[agents.explore] model = "…"`.
    /// A role not listed inherits the parent's model; `max_turns` defaults
    /// to 30.
    pub agents: HashMap<String, AgentConfig>,
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
            permissions: None,
            budgets: Budgets::default(),
            timeout_cap_seconds: 600,
            // 0 = no turn cap; budget caps and Esc are the real bounds (§9).
            max_turns_per_task: 0,
            verify: Vec::new(),
            typecheck: Vec::new(),
            sandbox: SandboxConfig::default(),
            network: NetworkConfig::default(),
            agents: HashMap::new(),
            compact_at: 0.75,
            context_window: crate::agent::prompt::DEFAULT_CONTEXT_WINDOW,
            compact_min_tokens: 24_000,
            lsp: HashMap::new(),
            lsp_check_edits: true,
        }
    }
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
    verify: Option<Vec<String>>,
    typecheck: Option<Vec<String>>,
    #[serde(default)]
    lsp: HashMap<String, String>,
}

/// Written once on first run (§7.1: model IDs live in the user's config,
/// never in code — this seed is theirs to edit). Must parse: the test holds
/// it to that.
const STARTER_CONFIG: &str = r#"# tursi configuration — created on first run; edit freely. SPEC.md documents it.
#
# API keys come from the environment (<PREFIX>_API_KEY, e.g. DEEPSEEK_API_KEY)
# or from ~/.tursi/secrets.toml (chmod 600):
#   [providers.deepseek]
#   key = "env:DEEPSEEK_API_KEY"

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

# Subagents (explore / review / worker) run on the main model unless a role
# names its own. Cheap-output models suit the read-heavy roles.
# [agents.explore]
# model = "deepseek/deepseek-flash"
# max_turns = 30

# Per-project .tursi/config.toml can override:
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
    /// and, forgivingly, the flat env-style form:
    ///   DEEPSEEK_API_KEY = "sk-…"   # <PREFIX>_API_KEY → provider <prefix>
    /// Values may be literal or `env:VAR_NAME`.
    pub(crate) fn load_from(path: &Path) -> Result<Secrets> {
        let mut secrets = Secrets { keys: HashMap::new(), base_urls: HashMap::new() };
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
                // Nested: [providers.<name>] key/base_url.
                toml::Value::Table(inner) if name == "providers" => {
                    for (provider, entry) in inner {
                        let Some(entry) = entry.as_table() else { continue };
                        if let Some(key) = entry.get("key").and_then(|k| k.as_str()).and_then(resolve) {
                            secrets.keys.insert(provider.clone(), key);
                        }
                        if let Some(url) = entry.get("base_url").and_then(|u| u.as_str()) {
                            secrets.base_urls.insert(provider.clone(), url.to_string());
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
    }

    #[cfg(unix)]
    fn set_600(path: &std::path::Path) {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600)).unwrap();
    }
    #[cfg(not(unix))]
    fn set_600(_: &std::path::Path) {}

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
