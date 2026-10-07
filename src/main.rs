//! tursi — autonomous coding harness. Design of record: SPEC.md.
//!
//! Built as its own §5.5 skeleton plan; the last stub filled with the UI
//! increment, so the skeleton-phase lint allows are gone.

mod agent;
mod api;
mod balance;
mod bus;
mod changes;
mod config;
mod dap;
mod deals;
mod decide;
mod diff;
mod headless;
mod ledger;
mod lsp;
mod monitor;
mod output;
mod plan;
mod ratelimit;
mod rizin;
mod router;
mod sandbox;
mod session;
mod shell;
mod stats;
mod syscard;
mod tools;
mod ui;
mod wire;

use anyhow::Result;
use clap::Parser;
use std::path::{Path, PathBuf};

#[derive(Parser)]
#[command(name = "tursi", about = "autonomous coding harness")]
struct Args {
    /// Project directory (defaults to the current directory)
    project: Option<PathBuf>,
    /// Resume a session of this project: the most recent one, or by id prefix
    #[arg(long, value_name = "ID", num_args = 0..=1, default_missing_value = "latest")]
    resume: Option<String>,
    /// List this project's sessions and exit
    #[arg(long)]
    sessions: bool,
    /// Start in AFK (unattended) mode (§4.4)
    #[arg(long)]
    afk: bool,
    /// Print a config/credential diagnosis and exit (never prints keys)
    #[arg(long)]
    check: bool,
    /// Run one task non-interactively (unattended), then exit — for scripting
    /// and benchmarking (BENCH.md). Implies --afk.
    #[arg(long, value_name = "PROMPT")]
    task: Option<String>,
    /// Like --task, but keep working until a separate evaluator judges this
    /// condition met (or impossible) — see `/goal`.
    #[arg(long, value_name = "CONDITION", conflicts_with = "task")]
    goal: Option<String>,
    /// With --task, emit metrics as a single JSON line.
    #[arg(long)]
    json: bool,
    /// Cap turns for this run (0 = unlimited). Overrides config — a safety
    /// bound for unattended/benchmark runs.
    #[arg(long)]
    max_turns: Option<u32>,
    /// Run on this model instead of the configured one (fallbacks are
    /// dropped). For comparing models; the model must have a credential.
    #[arg(long, value_name = "ID")]
    model: Option<String>,
    /// Run this model alone with full tools: no subagents, DEALS off. For
    /// DEALS warm-up runs (bench/deals-warmup.py) and model comparisons.
    #[arg(long, value_name = "ID", conflicts_with = "model")]
    station: Option<String>,
    /// Limit the DEALS pool to these stations for this run (comma-separated
    /// model ids), e.g. a single-model baseline that still goes through the
    /// pipeline (§5.8).
    #[arg(long, value_name = "IDS", conflicts_with = "station")]
    pool: Option<String>,
    /// Training runs: explore (`[deals] explore`), and route each new task to
    /// the least-tried station until every station has N outcomes
    /// (`[deals] explore_min`, §5.8; 0 explores without coverage).
    #[arg(long, value_name = "N")]
    explore_min: Option<u32>,
    /// Print the labels the decision model gives the brief on stdin (activity,
    /// domain, difficulty; §5.8) as one JSON line, `{}` when it can't, and
    /// exit. For building benchmark sets that cover them (bench/coverage.py).
    #[arg(long)]
    label: bool,
    /// List DEALS stations and what each has learned (§5.8); `probe`
    /// re-reads the provider's model catalog and tool-checks every candidate.
    #[arg(long, value_name = "probe", num_args = 0..=1, default_missing_value = "list")]
    stations: Option<String>,
    /// Run commands WITHOUT the sandbox — benchmarks inside a disposable
    /// container only (PERMISSIONS.md §6). Requires --task.
    #[arg(long)]
    no_sandbox: bool,
}

#[tokio::main]
async fn main() -> Result<()> {
    let args = Args::parse();
    if args.no_sandbox && args.task.is_none() && args.goal.is_none() {
        anyhow::bail!(
            "--no-sandbox is for benchmarks inside a disposable container only — it requires --task"
        );
    }
    // `--stations` and `--label` are about the account, not a project: they
    // must not plant a `.tursi/` in whatever directory they run from (a stray
    // one turns every directory below it into part of one project), so they
    // take no project root and log nowhere. Everything else logs to the
    // project ledger.
    let account_only = args.stations.is_some() || args.label;
    let project = if account_only { std::env::current_dir()? } else { project_root(args.project)? };
    if !account_only {
        init_logging();
    }
    let mut config = config::Config::load(&project)?;
    if let Some(max) = args.max_turns {
        config.max_turns_per_task = max;
    }
    if let Some(pool) = args.pool.as_deref() {
        config.deals.stations = pool.split(',').map(str::trim).filter(|m| !m.is_empty()).map(String::from).collect();
    }
    if let Some(n) = args.explore_min {
        config.deals.explore = true;
        config.deals.explore_min = n;
    }
    let secrets = config::Secrets::load()?;
    if args.check {
        return check(&project, &config, &secrets).await;
    }
    if args.label {
        use anyhow::Context;
        let mut brief = String::new();
        std::io::Read::read_to_string(&mut std::io::stdin(), &mut brief)?;
        let decider = decide::Decider::from_config(&config, &secrets).context("no decision model ([decide] model)")?;
        println!("{}", serde_json::to_string(&deals::labels::label(Some(&decider), &brief).await)?);
        return Ok(());
    }
    if let Some(mode) = args.stations.as_deref() {
        if !matches!(mode, "list" | "probe") {
            anyhow::bail!("--stations takes nothing or `probe`");
        }
        crate::ratelimit::configure(&config.limits, deals::catalog::Catalog::load().stations.iter().filter(|s| s.paid).map(|s| s.model.clone()));
        return deals::catalog::run_cli(&config, &secrets, mode == "probe").await;
    }
    if args.sessions {
        let list = session::Session::list(&project)?;
        if list.is_empty() {
            println!("no sessions yet");
        }
        for s in list.iter().rev().take(20) {
            println!(
                "{}  {}  {:<6} {:>4} msgs  {}",
                &s.id.to_string()[..13],
                s.started,
                if s.closed { "closed" } else { "open" },
                s.messages,
                s.first_prompt
            );
        }
        println!("\nresume with: tursi --resume <id-prefix>   (or --resume for the latest)");
        return Ok(());
    }
    if let Some(model) = args.model.clone() {
        config.model = model;
        config.fallbacks.clear();
    }
    if let Some(model) = args.station.clone() {
        if let Some(s) = deals::catalog::Catalog::load().get(&model) {
            config.prices.entry(model.clone()).or_insert_with(|| s.price());
            config.models.entry(model.clone()).or_insert_with(|| s.default_options());
        }
        config.model = model;
        config.fallbacks.clear();
        config.solo = true;
        config.deals.enabled = false;
    }
    if config.model.is_empty() {
        anyhow::bail!(
            "no model configured — set `model = \"<provider>/<model>\"` in {}/config.toml",
            config::Config::home_dir()?.display()
        );
    }
    let card = syscard::probe(&project, !args.no_sandbox);
    let session = session::Session::open_or_resume(&project, args.resume.as_deref())?;
    let sandbox = start_sandbox(&project, &config, session.id, args.no_sandbox)?;
    if let Some(task) = args.task {
        return headless::run(project, config, secrets, card, session, sandbox, headless::Job::Task(task), args.json).await;
    }
    if let Some(condition) = args.goal {
        return headless::run(project, config, secrets, card, session, sandbox, headless::Job::Goal(condition), args.json).await;
    }
    ui::run(project, config, secrets, card, session, sandbox, args.afk).await
}

/// The session's sandbox, or a refusal: tursi never runs commands unsandboxed
/// except under an explicit, headless-only `--no-sandbox` (PERMISSIONS.md §6).
fn start_sandbox(
    project: &Path,
    config: &config::Config,
    session: uuid::Uuid,
    no_sandbox: bool,
) -> Result<sandbox::Sandbox> {
    let options = sandbox::Options {
        project: project.to_path_buf(),
        session,
        timeout_cap: std::time::Duration::from_secs(config.timeout_cap_seconds),
        read_only: config::SandboxConfig::expand(&config.sandbox.read_only),
        read_write: config::SandboxConfig::expand(&config.sandbox.read_write),
        network_allow: config.network.allow.clone(),
    };
    if no_sandbox {
        eprintln!("warning: running WITHOUT the sandbox (--no-sandbox) — only inside a disposable container");
        return sandbox::Sandbox::unsandboxed(&options);
    }
    sandbox::Sandbox::start(&options).map_err(|e| {
        anyhow::anyhow!(
            "the sandbox could not start: {e:#}\n\ntursi runs every command inside a namespace \
             sandbox and needs unprivileged user namespaces (PERMISSIONS.md). Benchmarks inside \
             a disposable container can run headless with --task --no-sandbox."
        )
    })
}

/// `--check`: report what config and credentials resolve to, without ever
/// printing a key — the diagnostic for the credential snags that bite on
/// first setup.
async fn check(project: &Path, config: &config::Config, secrets: &config::Secrets) -> Result<()> {
    println!("project: {}", project.display());
    println!("config:  {}/config.toml", config::Config::home_dir()?.display());
    if config.model.is_empty() {
        println!("✗ no model configured");
        return Ok(());
    }
    let mut all_ok = true;
    let provider = |m: &str| m.split('/').next().unwrap_or("").to_string();
    println!("models:");
    for (role, model) in std::iter::once(("model", &config.model))
        .chain(config.fallbacks.iter().map(|f| ("fallback", f)))
        .chain(config.subagent.model.iter().map(|m| ("subagent", m)))
    {
        let cred = secrets.provider_for(model).is_some();
        let price = config.prices.contains_key(model);
        // Fallbacks reuse the task's provider client (agent::call_model);
        // subagents build their own.
        let same_provider = role != "fallback" || provider(model) == provider(&config.model);
        if !cred || !same_provider {
            all_ok = false;
        }
        println!(
            "  [{role}] {model}  credential:{}  price:{}{}",
            if cred { "found" } else { "MISSING" },
            if price { "set" } else { "unset (cost shows $0)" },
            if same_provider { "" } else { "  ✗ other provider — fallbacks must share the model's" },
        );
    }
    // Stats dir writability — a `chmod` mishap here silently loses cost
    // accounting (the task still runs; §9 bookkeeping is best-effort).
    let stats = config::Config::home_dir()?.join("stats");
    let writable = std::fs::create_dir_all(&stats).is_ok()
        && {
            let probe = stats.join(".write-probe");
            let ok = std::fs::write(&probe, b"").is_ok();
            let _ = std::fs::remove_file(&probe);
            ok
        };
    println!(
        "stats:   {} ({})",
        stats.display(),
        if writable { "writable" } else { "NOT writable — run: chmod 700 ~/.tursi ~/.tursi/stats" }
    );

    if let Some(balance) = crate::balance::Balance::from_config(&config, &secrets) {
        match balance.fetch().await {
            Ok(usd) => println!("balance: ${usd:.2} prepaid credit at the provider"),
            Err(e) => println!("balance: unavailable ({e:#})"),
        }
    }
    println!(
        "\n{}",
        if all_ok {
            "✓ ready — every model has a credential"
        } else {
            "✗ fix the models marked above — a missing credential: set <PREFIX>_API_KEY in the \
             environment or ~/.tursi/secrets.toml (nested [providers.<name>] key=, or flat \
             <PREFIX>_API_KEY=)"
        }
    );
    Ok(())
}

/// The project (PERMISSIONS.md §5.1): the start directory, unless an ancestor
/// already has a `.tursi/` — then that ancestor. No git repository needed.
fn project_root(cli: Option<PathBuf>) -> Result<PathBuf> {
    let start = match cli {
        Some(path) => path,
        None => std::env::current_dir()?,
    };
    let home = std::env::var_os("HOME").map(PathBuf::from).and_then(|h| h.canonicalize().ok());
    project_root_from(&start, home.as_deref())
}

/// `home` is excluded twice over: its `.tursi/` is the global config, not a
/// project marker; and a project at or above it would put all of $HOME —
/// ~/.ssh included — inside the sandbox.
fn project_root_from(start: &Path, home: Option<&Path>) -> Result<PathBuf> {
    use anyhow::Context;
    let start = start.canonicalize().with_context(|| format!("no such directory: {}", start.display()))?;
    let root = start
        .ancestors()
        .find(|dir| Some(*dir) != home && dir.join(".tursi").is_dir())
        .unwrap_or(&start)
        .to_path_buf();
    if home.is_some_and(|home| home.starts_with(&root)) {
        anyhow::bail!(
            "{} contains your home directory — start tursi in a project directory",
            root.display()
        );
    }
    std::fs::create_dir_all(root.join(".tursi"))?;
    // In a git repo, keep `git status` clean — a plain file append (§5.1).
    let info = root.join(".git/info");
    if root.join(".git").is_dir() && (info.is_dir() || std::fs::create_dir_all(&info).is_ok()) {
        let exclude = info.join("exclude");
        let current = std::fs::read_to_string(&exclude).unwrap_or_default();
        if !current.lines().any(|l| l.trim() == ".tursi/") {
            use std::io::Write;
            let mut file = std::fs::OpenOptions::new().create(true).append(true).open(&exclude)?;
            writeln!(file, ".tursi/")?;
        }
    }
    Ok(root)
}

/// Internal tracing → `trace` events in the project ledger (`ledger.rs`).
/// Events before the session opens are held and flushed when it does.
fn init_logging() {
    use tracing_subscriber::layer::SubscriberExt;
    let filter = tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info"));
    let subscriber = tracing_subscriber::registry().with(filter).with(ledger::TraceLayer);
    // Second init (tests) is fine — the first subscriber stays.
    let _ = tracing::subscriber::set_global_default(subscriber);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tools::testutil;

    #[test]
    fn project_root_needs_no_git_and_finds_the_nearest_tursi_dir() {
        let dir = testutil::tmp("root").canonicalize().unwrap();
        let home = dir.join("home");
        std::fs::create_dir_all(home.join(".tursi")).unwrap(); // the global config dir

        // A plain directory is a project; no exclude without a .git.
        let plain = home.join("plain");
        std::fs::create_dir_all(&plain).unwrap();
        assert_eq!(project_root_from(&plain, Some(&home)).unwrap(), plain);
        assert!(plain.join(".tursi").is_dir());

        // A subdirectory resolves to the ancestor holding .tursi/ …
        let sub = plain.join("src/deep");
        std::fs::create_dir_all(&sub).unwrap();
        assert_eq!(project_root_from(&sub, Some(&home)).unwrap(), plain);
        // … but never to $HOME itself, whose .tursi/ is the global config.
        let loose = home.join("loose/inner");
        std::fs::create_dir_all(&loose).unwrap();
        assert_eq!(project_root_from(&loose, Some(&home)).unwrap(), loose);

        // $HOME and its ancestors are refused outright.
        assert!(project_root_from(&home, Some(&home)).is_err());
        assert!(project_root_from(&dir, Some(&home)).is_err());

        // In a git repo, .tursi/ is excluded — once.
        let repo = home.join("repo");
        std::fs::create_dir_all(&repo).unwrap();
        assert!(std::process::Command::new("git").args(["init", "-q"]).current_dir(&repo).status().unwrap().success());
        project_root_from(&repo, Some(&home)).unwrap();
        project_root_from(&repo, Some(&home)).unwrap();
        let exclude = std::fs::read_to_string(repo.join(".git/info/exclude")).unwrap();
        assert_eq!(exclude.matches(".tursi/").count(), 1);
    }
}
