//! Stepped execution inside the session sandbox (§6, PERMISSIONS.md §2).
//! `init` is the sandbox's PID 1 and builds the view `mounts` plans; `client`
//! is the harness's handle on it; `protocol` is their wire.
//!
//! Everything that executes goes through [`Sandbox::spawn`]: the step shell,
//! ripgrep, language servers, gdb, rizin. The file tools don't spawn, but
//! [`Sandbox::resolve`] holds them to the same view.
//!
//! One shell per call (§6.2): steps are fed sequentially over stdin with
//! sentinel markers capturing per-step exit codes; each step's stdout/stderr
//! go to scratch files so the sentinel channel can't be polluted. Env
//! (`cd`, `export`) persists across steps and dies with the call.
//!
//! The shell is bash when installed, `sh` otherwise. Explicitly — `/bin/sh` is
//! dash on Debian/Ubuntu images, where bashisms models write (`cmd &>log`)
//! silently mean something else, and bash's PIPESTATUS lets a failing stage
//! upstream of `| tail` fail the step.

pub mod client;
mod init;
pub mod mounts;
pub mod protocol;
pub mod proxy;

use anyhow::{Context, Result, anyhow, bail};
use std::os::fd::OwnedFd;
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::{Component, Path, PathBuf};
use std::process::Stdio;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt, BufReader};

use protocol::What;

/// What a session's sandbox is built from.
pub struct Options {
    pub project: PathBuf,
    pub session: uuid::Uuid,
    /// Cap on per-step timeouts (config, default 600s).
    pub timeout_cap: Duration,
    /// Extra mounts from `[sandbox]` config (PERMISSIONS.md §7).
    pub read_only: Vec<PathBuf>,
    pub read_write: Vec<PathBuf>,
    /// Hosts reachable without asking, on top of the registries (§4.2).
    pub network_allow: Vec<String>,
}

/// A session's sandbox. Cheap to clone: one handle per owner (the toolbox,
/// the language-server manager, each debugger/rizin session); the sandbox
/// itself — and `/tmp/tursi-<uid>/<session>` — goes when the last drops.
#[derive(Clone)]
pub struct Sandbox {
    inner: Arc<Inner>,
    /// This handle's commands see the project read-only (the lead's, §5.7).
    project_read_only: bool,
}

/// Build-output directories a read-only project keeps writable, so a lead
/// or a reader can still build and test. These names are never anything
/// else; change capture skips them too (`changes.rs`).
pub(crate) const BUILD_DIRS: &[&str] = &[
    "target", "node_modules", "_build", "zig-out", ".zig-cache", ".venv", "venv", "__pycache__", ".pytest_cache",
    ".mypy_cache", ".ruff_cache", ".gradle", ".next", ".cache",
];
/// Names that are build output in one project and the deliverable in
/// another (a benchmark's `out/`, a site's `dist/`): writable for a
/// read-only project only when git ignores them, and always recorded.
const MAYBE_BUILD_DIRS: &[&str] = &["build", "dist", "out", "coverage"];

/// Directories a read-only project keeps writable: those of BUILD_DIRS
/// that exist, and those of MAYBE_BUILD_DIRS that exist and git ignores.
fn writable_dirs(project: &Path) -> Vec<PathBuf> {
    let mut out: Vec<PathBuf> = BUILD_DIRS.iter().map(|d| project.join(d)).filter(|p| p.is_dir()).collect();
    let maybe: Vec<&str> = MAYBE_BUILD_DIRS.iter().copied().filter(|d| project.join(d).is_dir()).collect();
    if !maybe.is_empty() && project.join(".git").exists() {
        // check-ignore prints the ignored ones of the paths it is given.
        let ignored = std::process::Command::new("git")
            .arg("-C")
            .arg(project)
            .args(["check-ignore", "--"])
            .args(maybe.iter().map(|d| format!("{d}/")))
            .output()
            .map(|o| String::from_utf8_lossy(&o.stdout).into_owned())
            .unwrap_or_default();
        let ignored: Vec<&str> = ignored.lines().map(|l| l.trim().trim_end_matches('/')).collect();
        out.extend(maybe.iter().filter(|d| ignored.contains(d)).map(|d| project.join(d)));
    }
    out
}

pub struct Inner {
    pub project: PathBuf,
    pub timeout_cap: Duration,
    /// The shell steps run in: bash's path when installed, else `sh`.
    pub shell: String,
    /// `shell` is bash: PIPESTATUS is recorded and bash syntax means bash.
    pub bash: bool,
    backend: Backend,
    /// Step output files, as the harness sees them and as the shell does.
    io_host: PathBuf,
    io_inside: PathBuf,
    /// `/tmp/tursi-<uid>/<session>`, removed when the sandbox drops.
    session_dir: PathBuf,
    /// The file tools' view (§2.2): each bind mount as (path inside, host
    /// source, writable); longest prefix wins. Empty = unsandboxed.
    view: Vec<View>,
    /// Masked paths (tursi's binaries): never visible.
    masks: Vec<PathBuf>,
    /// The network gate; None when unsandboxed.
    proxy: Option<proxy::Proxy>,
}

struct View {
    at: PathBuf,
    src: PathBuf,
    writable: bool,
}

impl std::ops::Deref for Sandbox {
    type Target = Inner;
    fn deref(&self) -> &Inner {
        &self.inner
    }
}

enum Backend {
    Namespaced(client::Namespaced),
    /// `--no-sandbox`: benchmarks inside a disposable container only (§6).
    Unsandboxed,
}

/// Where a spawned process's stdio goes.
pub enum Stream {
    Null,
    /// Connected to the returned `Child`.
    Piped,
    /// A host file (logs).
    File(std::fs::File),
}

pub struct Spawn {
    pub argv: Vec<String>,
    /// Added to (or overriding) the sandbox's base environment.
    pub env: Vec<(String, String)>,
    /// Defaults to the project root.
    pub cwd: Option<PathBuf>,
    pub stdin: Stream,
    pub stdout: Stream,
    pub stderr: Stream,
}

impl Spawn {
    /// Piped stdin/stdout, no stderr: the shape of every stdio-protocol child.
    pub fn piped(argv: Vec<String>) -> Spawn {
        Spawn { argv, env: vec![], cwd: None, stdin: Stream::Piped, stdout: Stream::Piped, stderr: Stream::Null }
    }
}

pub type In = Box<dyn AsyncWrite + Unpin + Send + Sync>;
pub type Out = Box<dyn AsyncRead + Unpin + Send + Sync>;

/// A process inside the sandbox. Killed when dropped.
pub struct Child {
    pub stdin: Option<In>,
    pub stdout: Option<Out>,
    pub stderr: Option<Out>,
    handle: Handle,
}

enum Handle {
    Inside { sandbox: Sandbox, proc: Option<client::Proc> },
    Local(tokio::process::Child),
}

impl Child {
    /// SIGKILL the process group.
    pub fn kill(&mut self) {
        match &mut self.handle {
            Handle::Inside { sandbox, proc } => {
                if let (Backend::Namespaced(ns), Some(proc)) = (&sandbox.backend, proc.as_ref()) {
                    ns.kill(proc.id);
                }
            }
            Handle::Local(child) => {
                if let Some(id) = child.id() {
                    let _ = nix::sys::signal::killpg(
                        nix::unistd::Pid::from_raw(id as i32),
                        nix::sys::signal::Signal::SIGKILL,
                    );
                }
                let _ = child.start_kill();
            }
        }
    }

    /// Exit code; None if a signal killed it or the sandbox died under it.
    pub async fn wait(&mut self) -> Option<i32> {
        match &mut self.handle {
            Handle::Inside { proc, .. } => match proc.take() {
                Some(proc) => proc.exited.await.ok().and_then(|e| e.code),
                None => None,
            },
            Handle::Local(child) => child.wait().await.ok().and_then(|s| s.code()),
        }
    }
}

impl Drop for Child {
    fn drop(&mut self) {
        self.kill();
    }
}

/// A finished one-shot command.
pub struct Output {
    pub code: Option<i32>,
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
}

#[derive(Debug, Clone)]
pub struct Step {
    pub command: String,
    /// Directory to run this step in (absolute), replacing `cd`. Isolated to
    /// the step — it does not leak to the next one (§4.1).
    pub cwd: Option<PathBuf>,
    /// Env vars for this step (validated keys), replacing `export`. Also
    /// step-local. Ordered for a deterministic script.
    pub env: Vec<(String, String)>,
    pub streams: Streams,
    pub timeout: Duration,
    pub tail_lines: usize,
}

/// Single-quote a string for safe interpolation into the shell script.
pub fn sh_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', "'\\''"))
}

/// Which output streams accompany the always-reported exit code + timing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Streams {
    /// Nothing extra on success; the error-aware extraction on failure (§4).
    Auto,
    /// No streams — the status line is the whole result.
    None,
    Stdout,
    Stderr,
    /// Both output streams (stdout and stderr).
    Both,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OnError {
    Stop,
    Continue,
}

/// Raw per-step facts; stream selection, truncation, and logging are the
/// exec tool's policy (§4.1) — the sandbox is mechanism only.
pub struct StepResult {
    pub command: String,
    /// None: killed by timeout, or the shell died (e.g. syntax error).
    pub exit_code: Option<i32>,
    pub elapsed: Duration,
    pub stdout: String,
    pub stderr: String,
    /// False when skipped by on_error=stop or an earlier shell death.
    pub ran: bool,
    /// Per-stage exit codes of the step's pipeline (bash only; empty
    /// otherwise). `exit_code` already folds them via `pipeline_exit`.
    pub pipe_status: Vec<i32>,
}

/// A pipeline's exit status: the last stage's, unless an earlier stage failed
/// while the last succeeded — `cargo test | tail` must not read as green.
/// SIGPIPE upstream (141) is how `| head` normally ends a producer: not a failure.
pub fn pipeline_exit(stages: &[i32]) -> i32 {
    let Some((&last, upstream)) = stages.split_last() else { return 0 };
    if last != 0 {
        return last;
    }
    upstream.iter().rev().copied().find(|&code| code != 0 && code != 141).unwrap_or(0)
}

impl Sandbox {
    /// Build this session's namespace sandbox. An error means there is none —
    /// the caller refuses to run rather than degrade (PERMISSIONS.md §6).
    pub fn start(o: &Options) -> Result<Sandbox> {
        let session_dir = session_dir(o.session)?;
        let started = (|| -> Result<(client::Namespaced, protocol::Spec, proxy::Proxy)> {
            mounts::prepare_project(&o.project)?;
            let home = std::env::var_os("HOME").map(PathBuf::from).filter(|h| h.is_absolute());
            let session = mounts::Session {
                tmp: session_dir.join("tmp"),
                io: session_dir.join("io"),
                root: session_dir.join("root"),
                proxy: session_dir.join("proxy"),
            };
            // The proxy listens before the init starts, so the forwarder's
            // first connection finds it.
            let gate = proxy::Proxy::start(&session.proxy.join(mounts::PROXY_SOCKET), &o.network_allow)?;
            let extra = mounts::Extra { read_only: &o.read_only, read_write: &o.read_write };
            let spec = mounts::plan(&o.project, home.as_deref(), &session, &extra, mounts::self_paths());
            let ns = client::Namespaced::start(&spec, &session_dir.join("init.log"))?;
            Ok((ns, spec, gate))
        })();
        match started {
            Ok((ns, spec, gate)) => Ok(Sandbox::with_backend(
                o,
                Backend::Namespaced(ns),
                session_dir.join("io"),
                mounts::IO_INSIDE.into(),
                session_dir,
                view_of(&spec),
                spec.masks.clone(),
                Some(gate),
            )),
            Err(e) => {
                let _ = std::fs::remove_dir_all(&session_dir);
                Err(e)
            }
        }
    }

    /// No sandbox: commands run directly on the host (`--no-sandbox`, §6).
    pub fn unsandboxed(o: &Options) -> Result<Sandbox> {
        let session_dir = session_dir(o.session)?;
        let io = session_dir.join("io");
        Ok(Sandbox::with_backend(o, Backend::Unsandboxed, io.clone(), io, session_dir, Vec::new(), Vec::new(), None))
    }

    #[allow(clippy::too_many_arguments)]
    fn with_backend(
        o: &Options,
        backend: Backend,
        io_host: PathBuf,
        io_inside: PathBuf,
        session_dir: PathBuf,
        view: Vec<View>,
        masks: Vec<PathBuf>,
        proxy: Option<proxy::Proxy>,
    ) -> Sandbox {
        let bash = crate::lsp::which("bash");
        Sandbox {
            inner: Arc::new(Inner {
                project: o.project.clone(),
                timeout_cap: o.timeout_cap,
                bash: bash.is_some(),
                shell: bash.unwrap_or_else(|| "sh".to_string()),
                backend,
                io_host,
                io_inside,
                session_dir,
                view,
                masks,
                proxy,
            }),
            project_read_only: false,
        }
    }

    /// The same sandbox, but commands run through this handle see the
    /// project read-only, build-output directories excepted. Enforced by the
    /// sandbox's mount namespace; a no-op unsandboxed.
    pub fn with_read_only_project(&self) -> Sandbox {
        Sandbox { inner: self.inner.clone(), project_read_only: true }
    }

    /// The per-spawn read-only spec: existing build directories stay
    /// writable (`writable_dirs`). A Rust project's `target/` is created
    /// first so the lead's first `cargo test` works.
    fn read_only_spec(&self) -> Option<protocol::ReadOnly> {
        if !self.project_read_only {
            return None;
        }
        if self.project.join("Cargo.toml").is_file() {
            let _ = std::fs::create_dir_all(self.project.join("target"));
        }
        Some(protocol::ReadOnly { project: self.project.clone(), writable: writable_dirs(&self.project) })
    }

    pub fn sandboxed(&self) -> bool {
        matches!(self.backend, Backend::Namespaced(_))
    }

    /// Full network for one call (PERMISSIONS.md §4.3). No-op unsandboxed.
    pub fn grant_network(&self, on: bool) {
        if let Some(p) = &self.proxy {
            p.grant_full(on);
        }
    }

    /// Hosts the gate refused since the last call.
    pub fn blocked_hosts(&self) -> Vec<String> {
        self.proxy.as_ref().map(|p| p.take_blocked()).unwrap_or_default()
    }

    /// Start a process inside the sandbox. Secret-shaped environment
    /// variables never enter; `spawn.env` is layered on top of the rest.
    pub async fn spawn(&self, spawn: Spawn) -> Result<Child> {
        let cwd = spawn.cwd.unwrap_or_else(|| self.project.clone());
        let mut env = base_env(self.sandboxed());
        if self.proxy.is_some() {
            // Everything that honors proxy variables goes through the gate;
            // the rest has no route at all (§4.1).
            let url = format!("http://127.0.0.1:{}", proxy::PORT);
            for key in ["HTTP_PROXY", "HTTPS_PROXY", "ALL_PROXY", "http_proxy", "https_proxy", "all_proxy"] {
                env.push((key.into(), url.clone()));
            }
            env.push(("NO_PROXY".into(), "127.0.0.1,localhost".into()));
            env.push(("no_proxy".into(), "127.0.0.1,localhost".into()));
        }
        for (k, v) in spawn.env {
            env.retain(|(key, _)| *key != k);
            env.push((k, v));
        }
        match &self.backend {
            Backend::Namespaced(ns) => {
                // Pipe ends: the child's go to the init, ours become tokio pipes.
                let (stdin_child, stdin_ours) = child_end(spawn.stdin, true)?;
                let (stdout_child, stdout_ours) = child_end(spawn.stdout, false)?;
                let (stderr_child, stderr_ours) = child_end(spawn.stderr, false)?;
                let proc = ns.spawn(spawn.argv, env, cwd, [stdin_child, stdout_child, stderr_child], self.read_only_spec()).await?;
                Ok(Child {
                    stdin: stdin_ours.map(|fd| Ok::<In, anyhow::Error>(Box::new(tokio::net::unix::pipe::Sender::from_owned_fd(fd)?))).transpose()?,
                    stdout: stdout_ours.map(|fd| Ok::<Out, anyhow::Error>(Box::new(tokio::net::unix::pipe::Receiver::from_owned_fd(fd)?))).transpose()?,
                    stderr: stderr_ours.map(|fd| Ok::<Out, anyhow::Error>(Box::new(tokio::net::unix::pipe::Receiver::from_owned_fd(fd)?))).transpose()?,
                    handle: Handle::Inside { sandbox: self.clone(), proc: Some(proc) },
                })
            }
            Backend::Unsandboxed => {
                let program = spawn.argv.first().ok_or_else(|| anyhow!("empty argv"))?.clone();
                let mut child = tokio::process::Command::new(program)
                    .args(&spawn.argv[1..])
                    .current_dir(&cwd)
                    .env_clear()
                    .envs(env)
                    .stdin(local_stdio(spawn.stdin))
                    .stdout(local_stdio(spawn.stdout))
                    .stderr(local_stdio(spawn.stderr))
                    .process_group(0)
                    .kill_on_drop(true)
                    .spawn()
                    .with_context(|| format!("spawning {}", spawn.argv[0]))?;
                Ok(Child {
                    stdin: child.stdin.take().map(|s| Box::new(s) as In),
                    stdout: child.stdout.take().map(|s| Box::new(s) as Out),
                    stderr: child.stderr.take().map(|s| Box::new(s) as Out),
                    handle: Handle::Local(child),
                })
            }
        }
    }

    /// Run to completion with captured output, bounded by `timeout` (killed
    /// on expiry, `code: None`).
    pub async fn output(&self, argv: Vec<String>, cwd: Option<PathBuf>, timeout: Duration) -> Result<Output> {
        let mut child = self
            .spawn(Spawn { argv, env: vec![], cwd, stdin: Stream::Null, stdout: Stream::Piped, stderr: Stream::Piped })
            .await?;
        let mut stdout = child.stdout.take().expect("piped");
        let mut stderr = child.stderr.take().expect("piped");
        let collect = async {
            let (mut out, mut err) = (Vec::new(), Vec::new());
            let (a, b) = tokio::join!(stdout.read_to_end(&mut out), stderr.read_to_end(&mut err));
            a?;
            b?;
            Ok::<_, std::io::Error>((out, err))
        };
        match tokio::time::timeout(timeout, collect).await {
            Ok(streams) => {
                let (stdout, stderr) = streams?;
                let code = child.wait().await;
                Ok(Output { code, stdout, stderr })
            }
            Err(_) => {
                child.kill();
                let _ = child.wait().await;
                Ok(Output { code: None, stdout: Vec::new(), stderr: b"killed: timed out".to_vec() })
            }
        }
    }

    /// The host path a model-supplied path denotes, if it exists in the
    /// sandbox's view (PERMISSIONS.md §2.2). Relative paths are project-
    /// relative. The path is mapped through the innermost mount it falls under
    /// (so `/tmp` is the session's, and a project that itself lives under
    /// `/tmp` still resolves to the project), symlinks are resolved, and the
    /// result is checked against the view again — a link can't lead out.
    /// Anything outside the view is "no such file". With `write`, the mount
    /// must be writable; for a new file the nearest existing ancestor decides.
    pub fn resolve(&self, path: &Path, write: bool) -> Result<PathBuf> {
        let abs = if path.is_absolute() { path.to_path_buf() } else { self.project.join(path) };
        if !self.sandboxed() {
            return canonical_prefix(&abs);
        }
        let missing = || anyhow!("no such file or directory: {}", path.display());
        let inside = self.innermost(&abs, |v| &v.at).ok_or_else(missing)?;
        let host = canonical_prefix(&inside.src.join(abs.strip_prefix(&inside.at).unwrap_or(Path::new(""))))?;
        if self.masks.iter().any(|m| host.starts_with(m)) {
            return Err(missing());
        }
        let landed = self.innermost(&host, |v| &v.src).ok_or_else(missing)?;
        if write && !landed.writable {
            bail!("{} is read-only in the sandbox", path.display());
        }
        Ok(host)
    }

    /// The deepest view entry whose `key` is a prefix of `path`.
    fn innermost(&self, path: &Path, key: impl Fn(&View) -> &Path) -> Option<&View> {
        self.view.iter().filter(|v| path.starts_with(key(v))).max_by_key(|v| key(v).as_os_str().len())
    }

    /// Where the step shell writes its per-step files for `scratch`.
    fn scratch_inside(&self, call_id: &str) -> PathBuf {
        self.io_inside.join(call_id)
    }

    /// Write a session-scoped scratch file the sandbox can read; returns the
    /// path as seen inside. (Background scripts, PERMISSIONS-era bench.)
    pub fn scratch_file(&self, name: &str, content: &str) -> Result<PathBuf> {
        std::fs::create_dir_all(&self.io_host)?;
        std::fs::write(self.io_host.join(name), content)?;
        Ok(self.io_inside.join(name))
    }

    /// For tests: the real sandbox, or — only when TURSI_TEST_UNSANDBOXED is
    /// set, on hosts without user namespaces — none.
    #[cfg(test)]
    pub fn for_tests(project: &Path) -> Sandbox {
        let o = Options {
            project: project.to_path_buf(),
            session: uuid::Uuid::now_v7(),
            timeout_cap: Duration::from_secs(600),
            read_only: vec![],
            read_write: vec![],
            network_allow: vec![],
        };
        if std::env::var_os("TURSI_TEST_UNSANDBOXED").is_some() {
            return Sandbox::unsandboxed(&o).unwrap();
        }
        Sandbox::start(&o).expect("sandbox start (set TURSI_TEST_UNSANDBOXED=1 on hosts without user namespaces)")
    }

    #[cfg(test)]
    pub fn with_shell(mut self, shell: &str, bash: bool) -> Sandbox {
        let inner = Arc::get_mut(&mut self.inner).expect("no other handles yet");
        inner.shell = shell.to_string();
        inner.bash = bash;
        self
    }

    /// Steps run sequentially in one shell; a step timeout SIGKILLs the whole
    /// tree and remaining steps report not run (§6.2).
    pub async fn run_steps(&self, steps: Vec<Step>, on_error: OnError) -> Result<Vec<StepResult>> {
        let call_id = uuid::Uuid::now_v7().simple().to_string();
        // The same directory twice: where the harness reads, where the shell writes.
        let scratch = self.io_host.join(&call_id);
        let scratch_in = self.scratch_inside(&call_id);
        std::fs::create_dir_all(&scratch)?;
        let shell_err_path = scratch.join("shell.err");

        let mut shell = self
            .spawn(Spawn {
                argv: vec![self.shell.clone()],
                env: vec![],
                cwd: None,
                stdin: Stream::Piped,
                stdout: Stream::Piped,
                stderr: Stream::File(std::fs::File::create(&shell_err_path)?),
            })
            .await
            .context("starting the shell")?;
        let mut stdin = shell.stdin.take().expect("piped");
        let mut lines = BufReader::new(shell.stdout.take().expect("piped")).lines();

        let mut results: Vec<StepResult> = Vec::with_capacity(steps.len());
        let mut dead = false;
        let mut stopped = false;
        for (i, step) in steps.iter().enumerate() {
            if dead || stopped {
                results.push(StepResult {
                    command: step.command.clone(),
                    exit_code: None,
                    elapsed: Duration::ZERO,
                    stdout: String::new(),
                    stderr: String::new(),
                    ran: false,
                    pipe_status: Vec::new(),
                });
                continue;
            }

            let [out_path, err_path, ps_path] = ["out", "err", "ps"].map(|ext| scratch.join(format!("{i}.{ext}")));
            let [out_in, err_in, ps_in] = ["out", "err", "ps"].map(|ext| scratch_in.join(format!("{i}.{ext}")));
            let sentinel = format!("__TURSI_{call_id}_{i}_");
            // Step stdin is /dev/null: nothing may block waiting for input.
            // A plain step runs in a brace group (current shell), so a bare
            // `cd`/`export` command still persists across steps. A step with
            // structured `cwd`/`env` runs in a subshell so those apply to it
            // alone and never leak to the next step (§4.1).
            let redir = format!("</dev/null >'{}' 2>'{}'", out_in.display(), err_in.display());
            // bash: record the pipeline's per-stage codes right after it (the
            // sentinel's $? would be this printf's). Absent file = the step
            // exited before reaching it (a failed `cd` in a subshell).
            let record = if self.bash {
                format!("\nprintf '%s' \"${{PIPESTATUS[*]}}\" >'{}'", ps_in.display())
            } else {
                String::new()
            };
            let body = if step.cwd.is_some() || !step.env.is_empty() {
                let mut pre = String::new();
                if let Some(cwd) = &step.cwd {
                    pre.push_str(&format!("cd {} || exit $?\n", sh_quote(&cwd.display().to_string())));
                }
                for (k, v) in &step.env {
                    pre.push_str(&format!("export {k}={}\n", sh_quote(v)));
                }
                format!("(\n{pre}{cmd}{record}\n) {redir}", cmd = step.command)
            } else {
                format!("{{\n{cmd}{record}\n}} {redir}", cmd = step.command)
            };
            let script = format!("{body}\nprintf '{sentinel}%d\\n' \"$?\"\n");

            let started = Instant::now();
            let deadline = started + step.timeout.min(self.timeout_cap);
            let mut exit_code: Option<i32> = None;
            if stdin.write_all(script.as_bytes()).await.is_err() {
                dead = true;
            } else {
                let _ = stdin.flush().await;
                loop {
                    let remaining = deadline.saturating_duration_since(Instant::now());
                    if remaining.is_zero() {
                        shell.kill();
                        dead = true;
                        break;
                    }
                    match tokio::time::timeout(remaining, lines.next_line()).await {
                        Err(_) => {
                            shell.kill();
                            dead = true;
                            break;
                        }
                        Ok(Ok(Some(line))) => {
                            if let Some(code) = line.strip_prefix(&sentinel) {
                                exit_code = code.trim().parse().ok();
                                break;
                            }
                        }
                        // Shell exited: syntax error or external kill.
                        Ok(Ok(None)) | Ok(Err(_)) => {
                            dead = true;
                            break;
                        }
                    }
                }
            }

            let pipe_status: Vec<i32> = std::fs::read_to_string(&ps_path)
                .unwrap_or_default()
                .split_whitespace()
                .filter_map(|code| code.parse().ok())
                .collect();
            if exit_code.is_some() && !pipe_status.is_empty() {
                exit_code = Some(pipeline_exit(&pipe_status));
            }
            let mut stderr = std::fs::read_to_string(&err_path).unwrap_or_default();
            if dead && exit_code.is_none() {
                let shell_err = std::fs::read_to_string(&shell_err_path).unwrap_or_default();
                if !shell_err.trim().is_empty() {
                    stderr.push_str(&format!("\n[shell]: {}", shell_err.trim()));
                }
            }
            results.push(StepResult {
                command: step.command.clone(),
                exit_code,
                elapsed: started.elapsed(),
                stdout: std::fs::read_to_string(&out_path).unwrap_or_default(),
                stderr,
                ran: true,
                pipe_status,
            });
            if on_error == OnError::Stop && exit_code != Some(0) {
                stopped = true;
            }
        }

        drop(stdin);
        // Leftovers in the call's process group go with it.
        shell.kill();
        let _ = shell.wait().await;
        let _ = std::fs::remove_dir_all(&scratch);
        Ok(results)
    }

}

impl Drop for Inner {
    fn drop(&mut self) {
        // Everything inside dies first; then its directories can go.
        if let Backend::Namespaced(ns) = &self.backend {
            ns.shutdown();
        }
        let _ = std::fs::remove_dir_all(&self.session_dir);
    }
}

/// The child's descriptor for one stream, plus our end when piped.
fn child_end(stream: Stream, child_reads: bool) -> Result<(OwnedFd, Option<OwnedFd>)> {
    Ok(match stream {
        Stream::Null => {
            let null = std::fs::OpenOptions::new().read(true).write(true).open("/dev/null")?;
            (null.into(), None)
        }
        Stream::File(file) => (file.into(), None),
        Stream::Piped => {
            let (read, write) = nix::unistd::pipe2(nix::fcntl::OFlag::O_CLOEXEC)?;
            if child_reads { (read, Some(write)) } else { (write, Some(read)) }
        }
    })
}

fn local_stdio(stream: Stream) -> Stdio {
    match stream {
        Stream::Null => Stdio::null(),
        Stream::Piped => Stdio::piped(),
        Stream::File(file) => Stdio::from(file),
    }
}

/// The file tools' view from the mount spec: the bind mounts. Tmpfs mounts
/// (the throwaway $HOME) have no host side, so paths under them that no
/// nested bind covers are invisible — exactly the §3 allowlist.
fn view_of(spec: &protocol::Spec) -> Vec<View> {
    spec.mounts
        .iter()
        .filter_map(|m| match &m.what {
            What::Bind { src, writable } => Some(View {
                at: m.at.clone(),
                src: src.canonicalize().unwrap_or_else(|_| src.clone()),
                writable: *writable,
            }),
            _ => None,
        })
        .collect()
}

/// Canonicalize the deepest existing ancestor and re-append the rest, so a
/// not-yet-created file resolves through the symlinks above it. Beyond the
/// existing part only plain names are allowed: `..` there could climb out of
/// the view (and the kernel would reject it anyway once the path exists).
fn canonical_prefix(abs: &Path) -> Result<PathBuf> {
    let components: Vec<Component> = abs.components().collect();
    let mut existing = components.len();
    while existing > 0 && std::fs::symlink_metadata(components[..existing].iter().collect::<PathBuf>()).is_err() {
        existing -= 1;
    }
    let (found, rest) = components.split_at(existing);
    if rest.iter().any(|c| !matches!(c, Component::Normal(_) | Component::CurDir)) {
        bail!("{}: path climbs through a directory that does not exist", abs.display());
    }
    let mut out = found
        .iter()
        .collect::<PathBuf>()
        .canonicalize()
        .with_context(|| format!("resolving {}", abs.display()))?;
    out.extend(rest.iter().filter(|c| matches!(c, Component::Normal(_))));
    Ok(out)
}

/// The environment for everything spawned: ours minus secret-shaped
/// variables and shell startup hooks; inside the sandbox, temp dirs point at
/// its /tmp.
fn base_env(inside: bool) -> Vec<(String, String)> {
    let secret = |key: &str| {
        let upper = key.to_ascii_uppercase();
        ["_API_KEY", "_TOKEN", "_SECRET", "PASSWORD"].iter().any(|s| upper.ends_with(s))
    };
    let proxy_var = |key: &str| key.to_ascii_lowercase().ends_with("_proxy");
    let mut env: Vec<(String, String)> = std::env::vars()
        .filter(|(k, _)| !secret(k) && k != "BASH_ENV" && k != "ENV")
        .filter(|(k, _)| !inside || !matches!(k.as_str(), "TMPDIR" | "XDG_RUNTIME_DIR") && !proxy_var(k))
        .collect();
    if inside {
        env.push(("TMPDIR".into(), "/tmp".into()));
    }
    env
}

/// `/tmp/tursi-<uid>/<session>` with tmp/, io/, root/. The parent must be
/// ours and private — on a shared /tmp anyone could pre-create it.
fn session_dir(session: uuid::Uuid) -> Result<PathBuf> {
    let uid = nix::unistd::getuid().as_raw();
    let top = PathBuf::from(format!("/tmp/tursi-{uid}"));
    match std::fs::create_dir(&top) {
        Ok(()) => std::fs::set_permissions(&top, std::fs::Permissions::from_mode(0o700))?,
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {}
        Err(e) => return Err(e).with_context(|| format!("creating {}", top.display())),
    }
    let meta = std::fs::symlink_metadata(&top)?;
    if !meta.is_dir() || meta.uid() != uid || meta.mode() & 0o077 != 0 {
        bail!("{} must be a directory you own with mode 0700 — refusing to use it", top.display());
    }
    let dir = top.join(session.to_string());
    for sub in ["tmp", "io", "root", "proxy"] {
        std::fs::create_dir_all(dir.join(sub))?;
    }
    Ok(dir)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("tursi-sbx-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn step(cmd: &str, secs: u64) -> Step {
        Step {
            command: cmd.to_string(),
            cwd: None,
            env: vec![],
            streams: Streams::Auto,
            timeout: Duration::from_secs(secs),
            tail_lines: 20,
        }
    }

    #[tokio::test]
    async fn env_and_cwd_persist_across_steps_within_one_call() {
        let dir = tmp("env");
        let sbx = Sandbox::for_tests(&dir);
        let results = sbx
            .run_steps(
                vec![
                    step("export TURSI_PROBE=42", 10),
                    step("echo $TURSI_PROBE", 10),
                    step("mkdir -p sub", 10),
                    step("cd sub", 10),
                    step("pwd", 10),
                ],
                OnError::Stop,
            )
            .await
            .unwrap();
        assert!(results.iter().all(|r| r.exit_code == Some(0)), "all steps green");
        assert_eq!(results[1].stdout.trim(), "42");
        assert!(results[4].stdout.trim().ends_with("/sub"));
    }

    #[tokio::test]
    async fn a_read_only_handle_cannot_write_the_project_but_can_build() {
        let dir = tmp("ro");
        std::fs::write(dir.join("a.txt"), "before").unwrap();
        std::fs::create_dir_all(dir.join("target")).unwrap();
        // `out/` could be the deliverable: read-only unless git ignores it,
        // as `build/` is here.
        for d in ["out", "build"] {
            std::fs::create_dir_all(dir.join(d)).unwrap();
        }
        std::fs::write(dir.join(".gitignore"), "build/\n").unwrap();
        let git = std::process::Command::new("git").arg("-C").arg(&dir).args(["init", "-q"]).status();
        assert!(git.is_ok_and(|s| s.success()));
        assert_eq!(writable_dirs(&dir), vec![dir.join("target"), dir.join("build")]);
        let sbx = Sandbox::for_tests(&dir);
        if !sbx.sandboxed() {
            return; // unsandboxed hosts cannot enforce it
        }
        let lead = sbx.with_read_only_project();
        let r = lead
            .run_steps(
                vec![
                    step("echo after > a.txt", 10),
                    step("python3 -c \"open('b.txt','w').write('x')\" || echo blocked", 10),
                    step("echo built > target/out && cat target/out", 10),
                    step("cat a.txt", 10),
                    step("echo x > out/result.json || echo blocked", 10),
                ],
                OnError::Continue,
            )
            .await
            .unwrap();
        assert_ne!(r[0].exit_code, Some(0), "shell redirect refused");
        assert!(r[0].stderr.contains("Read-only file system"), "{}", r[0].stderr);
        assert!(r[1].stdout.contains("blocked"), "interpreters are refused too");
        assert_eq!(r[2].stdout.trim(), "built", "build output stays writable: {}", r[2].stderr);
        assert_eq!(r[3].stdout.trim(), "before");
        assert!(r[4].stdout.contains("blocked"), "a deliverable out/ is read-only: {}", r[4].stdout);
        assert!(!dir.join("b.txt").exists());
        // The ordinary handle on the same sandbox still writes.
        let w = sbx.run_steps(vec![step("echo after > a.txt", 10)], OnError::Stop).await.unwrap();
        assert_eq!(w[0].exit_code, Some(0), "{}", w[0].stderr);
        assert_eq!(std::fs::read_to_string(dir.join("a.txt")).unwrap().trim(), "after");
    }

    #[tokio::test]
    async fn timeout_kills_and_remaining_steps_report_not_run() {
        let dir = tmp("timeout");
        let sbx = Sandbox::for_tests(&dir);
        let results = sbx
            .run_steps(vec![step("sleep 5", 1), step("echo after", 10)], OnError::Stop)
            .await
            .unwrap();
        assert_eq!(results[0].exit_code, None);
        assert!(results[0].ran);
        assert!(results[0].elapsed >= Duration::from_secs(1));
        assert!(!results[1].ran);
    }

    #[tokio::test]
    async fn cwd_and_env_apply_to_the_step_and_do_not_leak() {
        let dir = tmp("cwdenv");
        std::fs::create_dir_all(dir.join("sub")).unwrap();
        std::fs::write(dir.join("sub/marker.txt"), "x").unwrap();
        let sbx = Sandbox::for_tests(&dir);
        let mut s_cwd = step("ls", 10);
        s_cwd.cwd = Some(dir.join("sub"));
        let mut s_env = step("echo $TURSI_X", 10);
        s_env.env = vec![("TURSI_X".into(), "hello".into())];
        let r = sbx
            .run_steps(
                vec![
                    s_cwd,                     // 0: runs in sub/
                    step("pwd", 10),           // 1: must be back at root
                    s_env,                     // 2: sees TURSI_X
                    step("echo $TURSI_X", 10), // 3: must be empty
                ],
                OnError::Continue,
            )
            .await
            .unwrap();
        assert!(r[0].stdout.contains("marker.txt"), "cwd step: {:?}", r[0].stdout);
        assert!(!r[1].stdout.trim().ends_with("/sub"), "cwd leaked: {:?}", r[1].stdout);
        assert_eq!(r[2].stdout.trim(), "hello");
        assert_eq!(r[3].stdout.trim(), "", "env leaked to a later step");
    }

    #[test]
    fn pipeline_exit_fails_on_upstream_errors_but_not_sigpipe() {
        assert_eq!(pipeline_exit(&[101, 0]), 101, "cargo test | tail");
        assert_eq!(pipeline_exit(&[141, 0]), 0, "yes | head");
        assert_eq!(pipeline_exit(&[0, 1]), 1, "last stage failing wins");
        assert_eq!(pipeline_exit(&[2, 141, 0]), 2);
        assert_eq!(pipeline_exit(&[0]), 0);
    }

    #[tokio::test]
    async fn a_failing_stage_before_tail_fails_the_step_under_bash() {
        let dir = tmp("pipefail");
        let sbx = Sandbox::for_tests(&dir);
        if !sbx.bash {
            eprintln!("skipping: bash not installed");
            return;
        }
        let r = sbx
            .run_steps(
                vec![
                    step("printf 'test failed\\n' | tail -1 | (cat; exit 0) | false | cat", 10),
                    step("sh -c 'echo FAILED; exit 101' | tail -1", 10),
                    step("yes | head -2", 10),
                    step("cd no-such-dir", 10),
                ],
                OnError::Continue,
            )
            .await
            .unwrap();
        assert_eq!(r[0].exit_code, Some(1), "{:?}", r[0].pipe_status);
        assert_eq!(r[1].exit_code, Some(101), "upstream failure surfaces");
        assert_eq!(r[1].stdout.trim(), "FAILED");
        assert_eq!(r[2].exit_code, Some(0), "SIGPIPE upstream is not a failure");
        assert_eq!(r[3].exit_code, Some(1));
        // A cwd step whose `cd` fails exits before recording: sentinel code.
        let mut bad = step("true", 10);
        bad.cwd = Some(dir.join("missing"));
        let r = sbx.run_steps(vec![bad], OnError::Stop).await.unwrap();
        assert_ne!(r[0].exit_code, Some(0));
    }

    #[tokio::test]
    async fn plain_sh_fallback_reports_the_last_stage_and_records_nothing() {
        let dir = tmp("sh-fallback");
        let sbx = Sandbox::for_tests(&dir).with_shell("sh", false);
        let r = sbx
            .run_steps(vec![step("false | true", 10), step("echo $TURSI_NONE | cat", 10)], OnError::Continue)
            .await
            .unwrap();
        assert_eq!(r[0].exit_code, Some(0), "POSIX sh: last stage wins");
        assert!(r[0].pipe_status.is_empty());
        assert_eq!(r[1].exit_code, Some(0));
    }

    /// The allowlist view (PERMISSIONS.md §3), observed from inside.
    #[tokio::test]
    async fn the_view_hides_home_masks_tursi_and_protects_system_dirs() {
        let dir = tmp("view");
        let sbx = Sandbox::for_tests(&dir);
        if !sbx.sandboxed() {
            eprintln!("skipping: unsandboxed test run");
            return;
        }
        let home = std::env::var("HOME").unwrap();
        let exe = std::env::current_exe().unwrap().display().to_string();
        let r = sbx
            .run_steps(
                vec![
                    step(&format!("ls -A '{home}'"), 10),                 // 0: no dotfiles of the real home
                    step(&format!("test -e '{home}/.ssh'"), 10),          // 1: secrets absent
                    step(&format!("'{exe}' --help"), 10),                 // 2: tursi masked
                    step("touch /usr/x", 10),                             // 3: system read-only
                    step("touch /tmp/probe", 10),                         // 4: /tmp writable…
                    step("touch probe.txt", 10),                          // 5: …and so is the project
                    step("touch /tursi-nowhere", 10),                     // 6: unlisted root is read-only
                    step("cat /proc/1/comm", 10),                         // 7: own pid namespace
                    step("ls /proc | grep -c '^[0-9]' ", 10),             // 8: only sandbox processes
                ],
                OnError::Continue,
            )
            .await
            .unwrap();
        assert_eq!(r[0].exit_code, Some(0));
        assert!(!r[0].stdout.contains(".ssh") && !r[0].stdout.contains(".tursi"), "home leaked: {}", r[0].stdout);
        assert_ne!(r[1].exit_code, Some(0), ".ssh must not exist inside");
        assert_ne!(r[2].exit_code, Some(0), "tursi must not run inside: {}", r[2].stdout);
        assert_ne!(r[3].exit_code, Some(0), "/usr must be read-only");
        assert_eq!(r[4].exit_code, Some(0), "/tmp: {}", r[4].stderr);
        assert!(!Path::new("/tmp/probe").exists(), "the sandbox /tmp is not the host /tmp");
        assert_eq!(r[5].exit_code, Some(0), "project: {}", r[5].stderr);
        assert!(dir.join("probe.txt").exists());
        assert_ne!(r[6].exit_code, Some(0), "root must be read-only");
        // `tursi-<hash>` under cargo test, truncated to 15 chars by comm.
        assert!(r[7].stdout.starts_with("tursi"), "PID 1 is the init: {}", r[7].stdout);
        assert!(r[8].stdout.trim().parse::<u32>().unwrap() < 10, "host processes visible: {}", r[8].stdout);
    }

    /// Workload processes share one user namespace, so a debugger in one call
    /// can attach to a process another call left running (§2.3) — but never to
    /// the init (§2.4). PTRACE_ATTACH via python, the one dependency-free way
    /// to issue the raw call from a shell.
    #[tokio::test]
    async fn workload_processes_can_trace_each_other_but_not_the_init() {
        let dir = tmp("ptrace");
        let sbx = Sandbox::for_tests(&dir);
        if !sbx.sandboxed() || crate::lsp::which("python3").is_none() || crate::lsp::which("setsid").is_none() {
            eprintln!("skipping: needs the sandbox, python3, and setsid");
            return;
        }
        let attach = |pid: &str| {
            format!(
                "python3 -c 'import ctypes,sys; libc=ctypes.CDLL(None,use_errno=True); \
                 sys.exit(0 if libc.ptrace(16,int(sys.argv[1]),0,0)==0 else ctypes.get_errno())' {pid}"
            )
        };
        let r = sbx
            .run_steps(
                vec![
                    step("setsid -f sh -c 'echo $$ >/tmp/sleeper.pid; exec sleep 60'", 10),
                    // setsid -f returns before the child writes its pid: wait for it.
                    step("until test -s /tmp/sleeper.pid; do sleep 0.05; done", 10),
                ],
                OnError::Stop,
            )
            .await
            .unwrap();
        assert_eq!(r[0].exit_code, Some(0), "{}", r[0].stderr);
        assert_eq!(r[1].exit_code, Some(0), "the sleeper never wrote its pid");
        let r = sbx
            .run_steps(
                vec![
                    step("test -s /tmp/sleeper.pid", 10),
                    step(&attach("\"$(cat /tmp/sleeper.pid)\""), 10),
                    step(&attach("1"), 10),
                    step("readlink /proc/1/exe", 10),
                    step("kill $(cat /tmp/sleeper.pid)", 10),
                ],
                OnError::Continue,
            )
            .await
            .unwrap();
        assert_eq!(r[0].exit_code, Some(0), "the sleeper never started");
        assert_eq!(r[1].exit_code, Some(0), "attach to another call's process: {}", r[1].stderr);
        assert_eq!(r[2].exit_code, Some(1), "EPERM attaching to the init: {}", r[2].stderr);
        assert_ne!(r[3].exit_code, Some(0), "the init's exe must stay hidden: {}", r[3].stdout);
    }

    /// The file tools' view (PERMISSIONS.md §2.2): the same allowlist the
    /// mounts enforce, applied to model-supplied paths.
    #[test]
    fn resolve_maps_paths_through_the_view_and_hides_the_rest() {
        let dir = tmp("resolve");
        let sbx = Sandbox::for_tests(&dir);
        if !sbx.sandboxed() {
            eprintln!("skipping: unsandboxed test run");
            return;
        }
        let home = PathBuf::from(std::env::var("HOME").unwrap());
        // The project (which itself lives under /tmp) resolves to itself, writable.
        std::fs::write(dir.join("a.txt"), "x").unwrap();
        let canon = dir.canonicalize().unwrap();
        assert_eq!(sbx.resolve(Path::new("a.txt"), true).unwrap(), canon.join("a.txt"));
        assert_eq!(sbx.resolve(Path::new("new/deep/file.rs"), true).unwrap(), canon.join("new/deep/file.rs"));
        // /tmp is the session's private tmp, not the host's.
        let t = sbx.resolve(Path::new("/tmp/scratch.txt"), true).unwrap();
        assert!(t.starts_with(&sbx.session_dir), "{}", t.display());
        // .tursi is read-only; worktrees/ inside it is writable.
        assert!(sbx.resolve(Path::new(".tursi/x"), false).is_ok());
        assert!(sbx.resolve(Path::new(".tursi/x"), true).unwrap_err().to_string().contains("read-only"));
        assert!(sbx.resolve(Path::new(".tursi/worktrees/w/x"), true).is_ok());
        // System dirs: readable, not writable.
        assert!(sbx.resolve(Path::new("/etc/hostname"), false).is_ok());
        assert!(sbx.resolve(Path::new("/etc/hostname"), true).is_err());
        // Secrets and the rest of $HOME don't exist; tursi itself is masked.
        for hidden in [home.join(".ssh/id_ed25519"), home.join("Documents/x"), std::env::current_exe().unwrap()] {
            let err = sbx.resolve(&hidden, false).unwrap_err().to_string();
            assert!(err.contains("no such file"), "{}: {err}", hidden.display());
        }
        // A symlink out of the view is followed and then refused.
        std::os::unix::fs::symlink(home.join(".ssh"), dir.join("escape")).unwrap();
        assert!(sbx.resolve(Path::new("escape/id_ed25519"), false).unwrap_err().to_string().contains("no such file"));
        // `..` through a nonexistent directory can't climb out either.
        assert!(sbx.resolve(Path::new("nope/../../../../etc/passwd"), true).is_err());
    }

    /// PERMISSIONS.md §4: no route out, loopback up, the gate answers on
    /// :3128 and remembers what it refused.
    #[tokio::test]
    async fn network_is_loopback_only_and_the_gate_refuses_unknown_hosts() {
        let dir = tmp("net");
        let sbx = Sandbox::for_tests(&dir);
        if !sbx.sandboxed() {
            eprintln!("skipping: unsandboxed test run");
            return;
        }
        let r = sbx
            .run_steps(
                vec![
                    step("echo > /dev/tcp/1.1.1.1/80", 5),
                    step("echo $HTTPS_PROXY", 5),
                    step(
                        "exec 3<>/dev/tcp/127.0.0.1/3128; printf 'CONNECT blocked.example:443 HTTP/1.1\\r\\n\\r\\n' >&3; head -1 <&3",
                        10,
                    ),
                ],
                OnError::Continue,
            )
            .await
            .unwrap();
        assert_ne!(r[0].exit_code, Some(0), "direct connections must fail");
        assert_eq!(r[1].stdout.trim(), "http://127.0.0.1:3128");
        assert!(r[2].stdout.starts_with("HTTP/1.1 403"), "gate reply: {:?} {:?}", r[2].stdout, r[2].stderr);
        assert_eq!(sbx.blocked_hosts(), vec!["blocked.example:443".to_string()]);
        // A grant opens the gate (the upstream connect then fails: no such host → 502, not 403).
        sbx.grant_network(true);
        let r = sbx
            .run_steps(
                vec![step(
                    "exec 3<>/dev/tcp/127.0.0.1/3128; printf 'CONNECT blocked.example:443 HTTP/1.1\\r\\n\\r\\n' >&3; head -1 <&3",
                    30,
                )],
                OnError::Continue,
            )
            .await
            .unwrap();
        sbx.grant_network(false);
        assert!(!r[0].stdout.starts_with("HTTP/1.1 403"), "granted: {:?}", r[0].stdout);
        assert!(sbx.blocked_hosts().is_empty());
    }

    #[tokio::test]
    async fn on_error_stop_skips_and_continue_proceeds() {
        let dir = tmp("onerror");
        let sbx = Sandbox::for_tests(&dir);
        let stopped = sbx
            .run_steps(vec![step("false", 10), step("echo hi", 10)], OnError::Stop)
            .await
            .unwrap();
        assert_eq!(stopped[0].exit_code, Some(1));
        assert!(!stopped[1].ran);
        let continued = sbx
            .run_steps(vec![step("false", 10), step("echo hi", 10)], OnError::Continue)
            .await
            .unwrap();
        assert!(continued[1].ran);
        assert_eq!(continued[1].stdout.trim(), "hi");
    }
}
