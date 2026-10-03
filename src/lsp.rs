//! code_intel backend (§4.1, §8): one facade over per-language servers,
//! symbol-first addressing. Environment-correct resolution ladder (§8):
//! project `[lsp]` override → the project's own environment (.venv,
//! node_modules/.bin, compile_commands.json) → PATH. One server per
//! (project, language), owned by the per-project Toolbox — no global daemon.
//!
//! The client is deliberately lockstep: the executor is serial (§5.2), so a
//! request reads messages until its own response arrives, answering server
//! requests and absorbing notifications (diagnostics, progress) inline.

use anyhow::{Context, Result, anyhow, bail};
use serde::Deserialize;
use serde_json::{Value, json};
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};
use tokio::io::BufReader;

use crate::sandbox::{In, Out, Sandbox, Spawn, Stream};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Language {
    Rust,
    Python,
    TypeScript,
    C,
    Go,
}

#[derive(Debug, Clone, Copy, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Action {
    Definition,
    Hover,
    References,
    Diagnostics,
}

/// One query of a batched `code_intel` call (§4).
#[derive(Debug, Clone, Deserialize)]
pub struct Query {
    pub action: Action,
    pub symbol: Option<String>,
    pub file: Option<PathBuf>,
    pub line: Option<u32>,
    pub col: Option<u32>,
}

pub struct Manager {
    servers: HashMap<Language, Server>,
    project: PathBuf,
    /// Servers run inside the session sandbox (PERMISSIONS.md §2.2).
    sandbox: Sandbox,
    /// Per-language command overrides from `[lsp]` config (§8).
    overrides: HashMap<String, String>,
    /// Languages whose server we tried and failed to spawn in the best-effort
    /// post-edit path — don't re-pay a failing spawn on every subsequent edit.
    unavailable: HashSet<Language>,
}

struct Server {
    _child: crate::sandbox::Child,
    stdin: In,
    reader: BufReader<Out>,
    seq: i64,
    /// Answered back on workspace/configuration requests (pythonPath &c).
    settings: Value,
    /// Open documents → version, for didOpen/didChange.
    versions: HashMap<PathBuf, i64>,
    /// uri → rendered diagnostics, replaced on each publish.
    diags: HashMap<String, Vec<Diag>>,
    /// Active $/progress tokens; empty + quiet = settled.
    progress: HashSet<String>,
}

struct Diag {
    severity: i64,
    line: u64,
    message: String,
}

/// Resolution-ladder result: what to spawn and what to tell it.
struct Resolved {
    program: String,
    args: Vec<String>,
    envs: Vec<(String, String)>,
    settings: Value,
    init_options: Value,
}

impl Manager {
    pub fn new(project: PathBuf, overrides: HashMap<String, String>, sandbox: Sandbox) -> Manager {
        Manager { servers: HashMap::new(), project, sandbox, overrides, unavailable: HashSet::new() }
    }

    /// Symbol-first: resolve via workspace/symbol, fall back to file:line:col.
    /// Results are distillates per the minimal output contracts (§4).
    pub async fn query(&mut self, q: &Query) -> Result<String> {
        let lang = q
            .file
            .as_deref()
            .and_then(Self::detect)
            .or_else(|| self.project_language())
            .context("cannot infer the language — pass a file")?;
        self.ensure(lang).await?;
        let project = self.project.clone();
        let server = self.servers.get_mut(&lang).expect("ensured above");

        if let Action::Diagnostics = q.action {
            let file = q.file.clone().context("diagnostics needs a file")?;
            let path = absolutize(&project, &file);
            server.sync_doc(&path, lang).await?;
            server.settle(Duration::from_secs(3)).await?;
            return Ok(server.render_diags(&uri(&path), &project));
        }

        // Position: named symbol resolved via workspace/symbol, else explicit.
        let (path, line0, col0) = match &q.symbol {
            Some(symbol) => {
                let hits = server.workspace_symbols(symbol).await?;
                let picked = pick(&hits, symbol, q.file.as_deref());
                if picked.is_empty() {
                    return Ok(format!("no symbol named '{symbol}' in the workspace"));
                }
                // Declaration sites ARE the definition — answer directly.
                if let Action::Definition = q.action {
                    return Ok(render_hits(&picked, &project));
                }
                // workspace/symbol ranges start at the ITEM (the `fn`
                // keyword, column 0); positional requests need the cursor on
                // the identifier — find it in the declaration line.
                let hit = picked[0];
                let path = path_from_uri(&hit.uri);
                let col = std::fs::read_to_string(&path)
                    .ok()
                    .and_then(|s| s.lines().nth(hit.line as usize).map(str::to_string))
                    .and_then(|l| l.find(symbol.as_str()).map(|i| i as u64))
                    .unwrap_or(hit.col);
                (path, hit.line, col)
            }
            None => {
                let file = q.file.clone().context("give a symbol, or file+line")?;
                let line = q.line.context("positional queries need line")?;
                (absolutize(&project, &file), (line.max(1) - 1) as u64, q.col.unwrap_or(1).saturating_sub(1) as u64)
            }
        };
        server.sync_doc(&path, lang).await?;

        match q.action {
            Action::Hover => server.hover(&path, line0, col0).await,
            Action::References => server.references(&path, line0, col0, &project).await,
            Action::Definition => server.definition(&path, line0, col0, &project).await,
            Action::Diagnostics => unreachable!("handled above"),
        }
    }

    /// Post-edit hook (§3.1). With `spawn`, start the language server on first
    /// touch so type/import errors surface the moment they are written (a
    /// type-only import used as a value, an undefined symbol) — the agent need
    /// not opt in via code_intel. Without `spawn`, report only when a server is
    /// already running. Either way a missing/unspawnable server degrades to
    /// `None`, never an error: a container without the server installed still
    /// runs, and a failed spawn is remembered so edits don't retry it.
    pub async fn diagnostics_after_edit(&mut self, file: &Path, spawn: bool) -> Result<Option<String>> {
        let Some(lang) = Self::detect(file) else { return Ok(None) };
        if !self.servers.contains_key(&lang) {
            if !spawn || self.unavailable.contains(&lang) {
                return Ok(None);
            }
            if self.ensure(lang).await.is_err() {
                self.unavailable.insert(lang);
                return Ok(None);
            }
        }
        let project = self.project.clone();
        let path = absolutize(&project, file);
        let server = self.servers.get_mut(&lang).expect("checked above");
        server.sync_doc(&path, lang).await?;
        server.settle(Duration::from_secs(2)).await?;
        let diags = server.diags.get(&uri(&path)).map(Vec::as_slice).unwrap_or(&[]);
        let errors = diags.iter().filter(|d| d.severity == 1).count();
        let warnings = diags.iter().filter(|d| d.severity == 2).count();
        Ok(Some(if errors == 0 && warnings == 0 {
            "diagnostics: clean".to_string()
        } else {
            let first = diags
                .iter()
                .find(|d| d.severity == 1)
                .or_else(|| diags.first())
                .map(|d| truncate_line(&d.message, 100))
                .unwrap_or_default();
            format!("diagnostics: {errors} error(s), {warnings} warning(s) — first: {first}")
        }))
    }

    /// Spawn-on-demand + initialize + wait out initial indexing.
    async fn ensure(&mut self, lang: Language) -> Result<()> {
        if self.servers.contains_key(&lang) {
            return Ok(());
        }
        let resolved = resolve(&self.project, lang, &self.overrides)?;
        tracing::info!(?lang, program = %resolved.program, "spawning language server");

        let log_dir = self.project.join(".tursi");
        std::fs::create_dir_all(&log_dir)?;
        let stderr = std::fs::File::create(log_dir.join(format!("lsp-{lang:?}.log")))?;
        let mut argv = vec![resolved.program.clone()];
        argv.extend(resolved.args.iter().cloned());
        let mut child = self
            .sandbox
            .spawn(Spawn {
                argv,
                env: resolved.envs.clone(),
                cwd: None,
                stdin: Stream::Piped,
                stdout: Stream::Piped,
                stderr: Stream::File(stderr),
            })
            .await
            .with_context(|| format!("spawning {} — see the §8 ladder or set [lsp] in .tursi/config.toml", resolved.program))?;

        let stdin = child.stdin.take().context("lsp stdin")?;
        let reader = BufReader::new(child.stdout.take().context("lsp stdout")?);
        let mut server = Server {
            _child: child,
            stdin,
            reader,
            seq: 0,
            settings: resolved.settings,
            versions: HashMap::new(),
            diags: HashMap::new(),
            progress: HashSet::new(),
        };

        let init = json!({
            "processId": std::process::id(),
            "rootUri": uri(&self.project),
            "capabilities": {
                "window": {"workDoneProgress": true},
                "workspace": {"configuration": true, "symbol": {}},
                "textDocument": {
                    "hover": {"contentFormat": ["plaintext", "markdown"]},
                    "publishDiagnostics": {},
                    "definition": {}, "references": {}
                }
            },
            "initializationOptions": resolved.init_options,
        });
        server.request("initialize", init).await.context("initialize handshake")?;
        server.notify("initialized", json!({})).await?;
        if !server.settings.is_null() {
            server
                .notify("workspace/didChangeConfiguration", json!({"settings": server.settings.clone()}))
                .await?;
        }
        // First quiescence = initial indexing done. The grace floor matters:
        // servers go quiet for a beat BEFORE starting to index — returning
        // in that window leaves cross-file queries (references) empty.
        server.settle_min(Duration::from_secs(2), Duration::from_secs(60)).await?;
        self.servers.insert(lang, server);
        Ok(())
    }

    pub(crate) fn detect(file: &Path) -> Option<Language> {
        match file.extension()?.to_str()? {
            "rs" => Some(Language::Rust),
            "py" | "pyi" => Some(Language::Python),
            "ts" | "tsx" | "js" | "jsx" => Some(Language::TypeScript),
            "c" | "h" | "cc" | "cpp" | "hpp" => Some(Language::C),
            "go" => Some(Language::Go),
            _ => None,
        }
    }

    /// Symbol-only queries need a language: infer the project's primary one.
    fn project_language(&self) -> Option<Language> {
        let p = &self.project;
        if p.join("Cargo.toml").exists() {
            Some(Language::Rust)
        } else if p.join("pyproject.toml").exists() || p.join("uv.lock").exists() {
            Some(Language::Python)
        } else if p.join("package.json").exists() {
            Some(Language::TypeScript)
        } else if p.join("go.mod").exists() {
            Some(Language::Go)
        } else if p.join("compile_commands.json").exists() {
            Some(Language::C)
        } else {
            None
        }
    }
}

struct SymHit {
    name: String,
    uri: String,
    line: u64,
    col: u64,
}

/// Exact-name matches first; a file hint filters further (§4.1).
fn pick<'a>(hits: &'a [SymHit], symbol: &str, file: Option<&Path>) -> Vec<&'a SymHit> {
    let exact: Vec<&SymHit> = hits.iter().filter(|h| h.name == symbol).collect();
    let pool = if exact.is_empty() { hits.iter().collect() } else { exact };
    if let Some(file) = file {
        let hinted: Vec<&SymHit> = pool
            .iter()
            .copied()
            .filter(|h| h.uri.ends_with(&file.display().to_string()))
            .collect();
        if !hinted.is_empty() {
            return hinted;
        }
    }
    pool
}

fn render_hits(hits: &[&SymHit], project: &Path) -> String {
    let mut out = String::new();
    for hit in hits.iter().take(10) {
        let path = path_from_uri(&hit.uri);
        out.push_str(&format!(
            "{}:{}: {}\n",
            rel(project, &path),
            hit.line + 1,
            line_text(&path, hit.line)
        ));
    }
    if hits.len() > 10 {
        out.push_str(&format!("… {} more declarations\n", hits.len() - 10));
    }
    out.trim_end().to_string()
}

impl Server {
    async fn request(&mut self, method: &str, params: Value) -> Result<Value> {
        self.seq += 1;
        let id = self.seq;
        write_msg(&mut self.stdin, &json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params})).await?;
        let deadline = Instant::now() + Duration::from_secs(60);
        loop {
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                bail!("{method}: language server timed out");
            }
            let msg = tokio::time::timeout(remaining, read_msg(&mut self.reader))
                .await
                .map_err(|_| anyhow!("{method}: language server timed out"))??;
            if msg.get("id").and_then(Value::as_i64) == Some(id) && msg.get("method").is_none() {
                if let Some(err) = msg.get("error") {
                    bail!("{method}: {}", err.get("message").and_then(Value::as_str).unwrap_or("server error"));
                }
                return Ok(msg.get("result").cloned().unwrap_or(Value::Null));
            }
            self.handle_incoming(msg).await?;
        }
    }

    async fn notify(&mut self, method: &str, params: Value) -> Result<()> {
        write_msg(&mut self.stdin, &json!({"jsonrpc": "2.0", "method": method, "params": params})).await
    }

    /// Server requests get answered inline; notifications get absorbed.
    async fn handle_incoming(&mut self, msg: Value) -> Result<()> {
        if let Some(id) = msg.get("id").cloned() {
            let method = msg.get("method").and_then(Value::as_str).unwrap_or("");
            let result = match method {
                "workspace/configuration" => {
                    let items = msg.pointer("/params/items").and_then(Value::as_array).cloned().unwrap_or_default();
                    Value::Array(
                        items
                            .iter()
                            .map(|item| match item.get("section").and_then(Value::as_str) {
                                Some(section) => self
                                    .settings
                                    .pointer(&format!("/{}", section.replace('.', "/")))
                                    .cloned()
                                    .unwrap_or(Value::Null),
                                None => self.settings.clone(),
                            })
                            .collect(),
                    )
                }
                // workDoneProgress/create, registerCapability, refreshes, …
                _ => Value::Null,
            };
            write_msg(&mut self.stdin, &json!({"jsonrpc": "2.0", "id": id, "result": result})).await?;
            return Ok(());
        }
        match msg.get("method").and_then(Value::as_str) {
            Some("textDocument/publishDiagnostics") => {
                let uri = msg.pointer("/params/uri").and_then(Value::as_str).unwrap_or("").to_string();
                let diags = msg
                    .pointer("/params/diagnostics")
                    .and_then(Value::as_array)
                    .map(|list| {
                        list.iter()
                            .map(|d| Diag {
                                severity: d.get("severity").and_then(Value::as_i64).unwrap_or(1),
                                line: d.pointer("/range/start/line").and_then(Value::as_u64).unwrap_or(0),
                                message: d.get("message").and_then(Value::as_str).unwrap_or("").to_string(),
                            })
                            .collect()
                    })
                    .unwrap_or_default();
                self.diags.insert(uri, diags);
            }
            Some("$/progress") => {
                let token = msg.pointer("/params/token").map(Value::to_string).unwrap_or_default();
                match msg.pointer("/params/value/kind").and_then(Value::as_str) {
                    Some("begin") => {
                        self.progress.insert(token);
                    }
                    Some("end") => {
                        self.progress.remove(&token);
                    }
                    _ => {}
                }
            }
            _ => {}
        }
        Ok(())
    }

    /// Drain messages until no active progress and a quiet gap — the §8
    /// settle strategy — bounded by `max`.
    async fn settle(&mut self, max: Duration) -> Result<()> {
        self.settle_min(Duration::ZERO, max).await
    }

    async fn settle_min(&mut self, min: Duration, max: Duration) -> Result<()> {
        let start = Instant::now();
        let deadline = start + max;
        loop {
            if Instant::now() >= deadline {
                return Ok(());
            }
            match tokio::time::timeout(Duration::from_millis(300), read_msg(&mut self.reader)).await {
                Ok(msg) => self.handle_incoming(msg?).await?,
                Err(_) => {
                    if self.progress.is_empty() && start.elapsed() >= min {
                        return Ok(());
                    }
                }
            }
        }
    }

    /// didOpen on first contact, didChange (full text) after — the server
    /// always analyzes what's on disk right now.
    async fn sync_doc(&mut self, path: &Path, lang: Language) -> Result<()> {
        let text = std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
        match self.versions.get_mut(path) {
            None => {
                self.versions.insert(path.to_path_buf(), 1);
                self.notify(
                    "textDocument/didOpen",
                    json!({"textDocument": {"uri": uri(path), "languageId": lang_id(lang), "version": 1, "text": text}}),
                )
                .await
            }
            Some(version) => {
                *version += 1;
                let v = *version;
                self.notify(
                    "textDocument/didChange",
                    json!({"textDocument": {"uri": uri(path), "version": v}, "contentChanges": [{"text": text}]}),
                )
                .await
            }
        }
    }

    async fn workspace_symbols(&mut self, name: &str) -> Result<Vec<SymHit>> {
        let result = self.request("workspace/symbol", json!({"query": name})).await?;
        let hits = result
            .as_array()
            .map(|list| {
                list.iter()
                    .filter_map(|s| {
                        Some(SymHit {
                            name: s.get("name")?.as_str()?.to_string(),
                            uri: s.pointer("/location/uri")?.as_str()?.to_string(),
                            line: s.pointer("/location/range/start/line").and_then(Value::as_u64).unwrap_or(0),
                            col: s.pointer("/location/range/start/character").and_then(Value::as_u64).unwrap_or(0),
                        })
                    })
                    .collect()
            })
            .unwrap_or_default();
        Ok(hits)
    }

    async fn hover(&mut self, path: &Path, line: u64, col: u64) -> Result<String> {
        let result = self
            .request("textDocument/hover", position_params(path, line, col))
            .await?;
        let text = match result.pointer("/contents") {
            None => return Ok("no hover information".to_string()),
            Some(Value::String(s)) => s.clone(),
            Some(c) => c
                .pointer("/value")
                .and_then(Value::as_str)
                .map(str::to_string)
                .or_else(|| {
                    c.as_array().map(|parts| {
                        parts
                            .iter()
                            .filter_map(|p| p.as_str().map(str::to_string).or_else(|| p.pointer("/value").and_then(Value::as_str).map(str::to_string)))
                            .collect::<Vec<_>>()
                            .join("\n")
                    })
                })
                .unwrap_or_default(),
        };
        let text: String = text.lines().take(40).collect::<Vec<_>>().join("\n");
        Ok(if text.trim().is_empty() { "no hover information".to_string() } else { text })
    }

    async fn references(&mut self, path: &Path, line: u64, col: u64, project: &Path) -> Result<String> {
        let mut params = position_params(path, line, col);
        params["context"] = json!({"includeDeclaration": false});
        let mut out =
            render_locations(&self.request("textDocument/references", params.clone()).await?, project, 30);
        // Empty during late indexing: settle and ask once more.
        if out == "no locations" {
            self.settle(Duration::from_secs(3)).await?;
            out = render_locations(&self.request("textDocument/references", params).await?, project, 30);
        }
        Ok(out)
    }

    async fn definition(&mut self, path: &Path, line: u64, col: u64, project: &Path) -> Result<String> {
        let params = position_params(path, line, col);
        let mut out =
            render_locations(&self.request("textDocument/definition", params.clone()).await?, project, 10);
        if out == "no locations" {
            self.settle(Duration::from_secs(3)).await?;
            out = render_locations(&self.request("textDocument/definition", params).await?, project, 10);
        }
        Ok(out)
    }

    fn render_diags(&self, uri: &str, project: &Path) -> String {
        let Some(diags) = self.diags.get(uri).filter(|d| !d.is_empty()) else {
            return "no diagnostics from the language server (the compiler remains the authority)".to_string();
        };
        let mut out = String::new();
        for d in diags.iter().take(30) {
            let sev = match d.severity {
                1 => "error",
                2 => "warning",
                3 => "info",
                _ => "hint",
            };
            out.push_str(&format!("{sev} L{}: {}\n", d.line + 1, truncate_line(&d.message, 200)));
        }
        if diags.len() > 30 {
            out.push_str(&format!("… {} more\n", diags.len() - 30));
        }
        let _ = project;
        out.trim_end().to_string()
    }
}

/// Locations arrive as Location, Location[], or LocationLink[] — render all.
fn render_locations(result: &Value, project: &Path, cap: usize) -> String {
    let mut locations: Vec<(PathBuf, u64)> = Vec::new();
    let mut push = |v: &Value| {
        let uri = v.get("uri").or_else(|| v.get("targetUri")).and_then(Value::as_str);
        let line = v
            .pointer("/range/start/line")
            .or_else(|| v.pointer("/targetRange/start/line"))
            .and_then(Value::as_u64);
        if let (Some(uri), Some(line)) = (uri, line) {
            locations.push((path_from_uri(uri), line));
        }
    };
    match result {
        Value::Array(list) => list.iter().for_each(&mut push),
        v @ Value::Object(_) => push(v),
        _ => {}
    }
    if locations.is_empty() {
        return "no locations".to_string();
    }
    let total = locations.len();
    let mut out = String::new();
    for (path, line) in locations.into_iter().take(cap) {
        out.push_str(&format!("{}:{}: {}\n", rel(project, &path), line + 1, line_text(&path, line)));
    }
    if total > cap {
        out.push_str(&format!("… {} more\n", total - cap));
    }
    out.trim_end().to_string()
}

/// The §8 environment ladder: override → project env → PATH.
fn resolve(project: &Path, lang: Language, overrides: &HashMap<String, String>) -> Result<Resolved> {
    let key = match lang {
        Language::Rust => "rust",
        Language::Python => "python",
        Language::TypeScript => "typescript",
        Language::C => "c",
        Language::Go => "go",
    };
    let mut resolved = Resolved {
        program: String::new(),
        args: vec![],
        envs: vec![],
        settings: Value::Null,
        init_options: Value::Null,
    };

    // Python: env correctness is settings + env, independent of the binary.
    if lang == Language::Python {
        let venv = project.join(".venv");
        if venv.is_dir() {
            resolved.settings = json!({"python": {"pythonPath": venv.join("bin/python").display().to_string()}});
            let path = std::env::var("PATH").unwrap_or_default();
            resolved.envs = vec![
                ("VIRTUAL_ENV".into(), venv.display().to_string()),
                ("PATH".into(), format!("{}:{path}", venv.join("bin").display())),
            ];
        }
    }

    if let Some(command) = overrides.get(key) {
        let mut parts = command.split_whitespace();
        resolved.program = parts.next().context("empty [lsp] override")?.to_string();
        resolved.args = parts.map(str::to_string).collect();
        return Ok(resolved);
    }

    match lang {
        Language::Rust => resolved.program = which("rust-analyzer").context("rust-analyzer not found — `rustup component add rust-analyzer`")?,
        Language::Go => resolved.program = which("gopls").context("gopls not found")?,
        Language::Python => {
            let venv_bin = project.join(".venv/bin");
            resolved.program = ["basedpyright-langserver", "pyright-langserver"]
                .iter()
                .find_map(|name| {
                    let local = venv_bin.join(name);
                    if local.is_file() { Some(local.display().to_string()) } else { which(name) }
                })
                .context("no Python language server — install basedpyright/pyright or set [lsp] python")?;
            resolved.args = vec!["--stdio".into()];
        }
        Language::TypeScript => {
            let local = project.join("node_modules/.bin/typescript-language-server");
            resolved.program = if local.is_file() {
                local.display().to_string()
            } else {
                which("typescript-language-server").context("typescript-language-server not found")?
            };
            resolved.args = vec!["--stdio".into()];
            let workspace_ts = project.join("node_modules/typescript/lib");
            if workspace_ts.is_dir() {
                resolved.init_options = json!({"tsserver": {"path": workspace_ts.display().to_string()}});
            }
        }
        Language::C => {
            resolved.program = which("clangd").context("clangd not found")?;
            for dir in [".", "build", "out", "target"] {
                if project.join(dir).join("compile_commands.json").is_file() {
                    resolved.args = vec![format!("--compile-commands-dir={}", project.join(dir).display())];
                    break;
                }
            }
        }
    }
    Ok(resolved)
}

pub(crate) fn which(name: &str) -> Option<String> {
    let path = std::env::var_os("PATH")?;
    for dir in std::env::split_paths(&path) {
        let candidate = dir.join(name);
        if candidate.is_file() {
            return Some(candidate.display().to_string());
        }
    }
    None
}

fn lang_id(lang: Language) -> &'static str {
    match lang {
        Language::Rust => "rust",
        Language::Python => "python",
        Language::TypeScript => "typescript",
        Language::C => "c",
        Language::Go => "go",
    }
}

fn position_params(path: &Path, line: u64, col: u64) -> Value {
    json!({"textDocument": {"uri": uri(path)}, "position": {"line": line, "character": col}})
}

fn uri(path: &Path) -> String {
    format!("file://{}", path.display())
}

fn path_from_uri(uri: &str) -> PathBuf {
    PathBuf::from(uri.strip_prefix("file://").unwrap_or(uri))
}

fn rel(project: &Path, path: &Path) -> String {
    path.strip_prefix(project).unwrap_or(path).display().to_string()
}

fn line_text(path: &Path, line0: u64) -> String {
    std::fs::read_to_string(path)
        .ok()
        .and_then(|s| s.lines().nth(line0 as usize).map(|l| l.trim().to_string()))
        .unwrap_or_default()
}

fn truncate_line(s: &str, max: usize) -> String {
    let line = s.lines().next().unwrap_or("");
    line.chars().take(max).collect()
}

use crate::wire::{read_msg, write_msg};

fn absolutize(project: &Path, file: &Path) -> PathBuf {
    if file.is_absolute() { file.to_path_buf() } else { project.join(file) }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tools::testutil;

    fn have_ra() -> bool {
        std::process::Command::new("rust-analyzer")
            .arg("--version")
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false)
    }

    fn mini_crate(dir: &Path, main_rs: &str) {
        std::fs::write(dir.join("Cargo.toml"), "[package]\nname = \"mini\"\nversion = \"0.1.0\"\nedition = \"2021\"\n").unwrap();
        std::fs::create_dir_all(dir.join("src")).unwrap();
        std::fs::write(dir.join("src/main.rs"), main_rs).unwrap();
    }

    fn by_symbol(action: Action, symbol: &str) -> Query {
        Query { action, symbol: Some(symbol.to_string()), file: None, line: None, col: None }
    }

    #[tokio::test]
    async fn symbol_first_definition_hover_references_against_rust_analyzer() {
        if !have_ra() {
            eprintln!("skipping: rust-analyzer not installed");
            return;
        }
        let dir = testutil::tmp("lsp-ra");
        // Call site outside any macro: reference-finding must not depend on
        // stdlib macro expansion (rust-src may be missing on the host).
        mini_crate(
            &dir,
            "fn greet(name: &str) -> String {\n    format!(\"hello {name}\")\n}\n\nfn main() {\n    let g = greet(\"tursi\");\n    println!(\"{g}\");\n}\n",
        );
        let mut m = Manager::new(dir.clone(), HashMap::new(), Sandbox::for_tests(&dir));

        let def = m.query(&by_symbol(Action::Definition, "greet")).await.unwrap();
        assert!(def.contains("src/main.rs:1"), "definition: {def}");
        assert!(def.contains("fn greet"), "definition line text: {def}");

        let hover = m.query(&by_symbol(Action::Hover, "greet")).await.unwrap();
        assert!(hover.contains("greet") && hover.contains("String"), "hover: {hover}");

        let refs = m.query(&by_symbol(Action::References, "greet")).await.unwrap();
        assert!(refs.contains("src/main.rs:6"), "call site: {refs}");
        assert!(refs.contains("greet(\"tursi\")"), "call text: {refs}");

        let missing = m.query(&by_symbol(Action::Definition, "no_such_fn_xyz")).await.unwrap();
        assert!(missing.contains("no symbol named"));
    }

    #[tokio::test]
    async fn diagnostics_reports_a_type_error_after_sync() {
        if !have_ra() {
            eprintln!("skipping: rust-analyzer not installed");
            return;
        }
        let dir = testutil::tmp("lsp-diag");
        mini_crate(&dir, "fn main() {\n    let x: u32 = \"nope\";\n    let _ = x;\n}\n");
        let mut m = Manager::new(dir.clone(), HashMap::new(), Sandbox::for_tests(&dir));
        let out = m
            .query(&Query {
                action: Action::Diagnostics,
                symbol: None,
                file: Some(PathBuf::from("src/main.rs")),
                line: None,
                col: None,
            })
            .await
            .unwrap();
        assert!(out.contains("error"), "diagnostics: {out}");
        assert!(out.contains("L2") || out.to_lowercase().contains("expected"), "location/type: {out}");
    }

    #[tokio::test]
    async fn diagnostics_after_edit_autospawns_and_catches_a_type_error() {
        // The drizzle-class fix: a type/import error must surface right after
        // the file is written, WITHOUT the agent first opening a server via
        // code_intel. Auto-spawn on the post-edit hook makes that happen.
        if !have_ra() {
            eprintln!("skipping: rust-analyzer not installed");
            return;
        }
        let dir = testutil::tmp("lsp-postedit");
        mini_crate(&dir, "fn main() {\n    let _x: u32 = \"nope\";\n}\n");
        let sandbox = Sandbox::for_tests(&dir);
        let mut m = Manager::new(dir.clone(), HashMap::new(), sandbox.clone());
        // No server running yet — spawn=true must start one and report the error.
        let out = m.diagnostics_after_edit(&PathBuf::from("src/main.rs"), true).await.unwrap();
        assert!(
            matches!(out.as_deref(), Some(s) if s.contains("error")),
            "post-edit diagnostics should auto-spawn and flag the type error: {out:?}"
        );

        // spawn=false with nothing running degrades to None (the opt-in path).
        let mut m2 = Manager::new(dir, HashMap::new(), sandbox);
        let none = m2.diagnostics_after_edit(&PathBuf::from("src/main.rs"), false).await.unwrap();
        assert!(none.is_none(), "no server + spawn=false → None, got {none:?}");
    }
}
