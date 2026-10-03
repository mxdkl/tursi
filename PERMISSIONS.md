# tursi — permissions & containment

> **Status:** implemented 2026-10-03 (all four phases of §9). SPEC.md defers to this document where they overlap. Once built, it supersedes
> SPEC §0 principle 1 (edit in place) where it conflicts, §3.4 (approval modes), §6.2
> (sandboxing), and the sandbox/attach notes in §4.3. SPEC.md gets folded in once the code
> lands.

## 0. Principles

1. **The sandbox is the wall.** Every boundary is enforced by the kernel (namespaces),
   never by pattern-matching command strings. A trick that fools a parser cannot cross it.
2. **Full autonomy inside, decisions stay with the user.** There is one mode — the agent
   runs everything in the sandbox without asking. Autonomy is about *permission to act*,
   not *authority to decide*: the model still brings decisions to the user (§1.2).
3. **Git is the agent's tool, not the harness's.** tursi has no git commands and does not
   require a repository. Projects are directories; in a git repo, the agent isolates its own
   work in a worktree because the prompt tells it to (§5.2), the way a careful engineer
   would.
4. **Fail closed.** No sandbox, no session — except an explicit benchmark-only escape (§6).

Threat model: a well-meaning model that hostile project content can steer (a README or
test fixture instructing it to fetch or exfiltrate). Both mistakes and injection must stay
inside the wall. Inside the project, protection is behavioral (§5) plus edit checkpoints.

## 1. Modes and questions

### 1.1 One mode

There is no approval mode. Edits, commands, builds, tests, debugging — all run without a
prompt. The **only** permission prompt is network access beyond the allowed domains (§4).
Plan mode (§5.5 skeletons) is unaffected. Removed: NORMAL/AUTO, `:auto`/`:normal`, the
diff and command approval overlays, `a` (always-allow), read-only mode.

AFK stays as a separate switch meaning "no one is there to answer" (§1.3).

### 1.2 What the model must always ask

In every situation, the model stops and asks (`ask_user`) before:

- **Unclear requirements** — the task reads more than one way, or the scope is unclear.
- **Destructive or irreversible steps** — deleting files or data, migrations, rewriting
  history, force operations, removing tests.

This is a prompt rule (the model consulting the user), not a harness gate.

### 1.3 When no one can answer

AFK and headless runs: `ask_user` returns immediately; the model proceeds on its best
judgment, logs the assumption, and lists every assumption in its final report (today's
§4.4 behavior). Network prompts are refused, and the model is told (§4.3).

## 2. The sandbox

### 2.1 Namespaces and lifetime

- **Namespaces:** user, mount, pid, net, ipc, uts. Not cgroup, not time.
- **One sandbox per session.** It is created at session start and torn down at session end;
  state survives across calls (a server started in one call is there for the next).
- **Init:** tursi re-executes its own binary as the sandbox's PID 1
  (`tursi __sandbox <spec>`), which builds the mounts, drops privileges, and then serves
  spawn requests from the harness. No dependency on `unshare` or `bwrap` — the single
  binary works wherever it is copied. When the init dies, the kernel kills everything in
  the namespace.
- **Two user namespaces.** The init lives in an outer user namespace; everything it spawns
  (the workload) runs in a nested one. The init makes the nested namespace once, before
  anything runs inside, and every spawn joins it before it execs — so all workload
  processes share it. Capabilities granted to the workload (§2.3) exist only in the inner
  namespace and confer nothing over the init (§2.4).

### 2.2 What runs inside

Everything that executes: `execute_command` steps, `search` (ripgrep), language servers
(`code_intel` and post-edit diagnostics), gdb (`debug`), rizin, `profile`, custom tools.
rizin's `!` and gdb's `shell` are therefore contained — no longer permission holes.

The file tools (`read`, `write`, `edit`) run in the harness process but enforce the same
view: paths are resolved (symlinks included) and checked against the mount table; anything
outside it is reported as "no such file". `/tmp` paths are translated to the session's
tmp directory (§3).

The harness itself stays **outside**: the API key, the cost ledger, sessions, and config.
The sandbox's fresh `/proc` shows only sandbox processes, so tursi (and its environment)
is invisible from inside.

### 2.3 Identity and limits

- **Your own uid** inside, not root. All capabilities are dropped once the mounts are built,
  except `CAP_SYS_PTRACE` in the workload's (inner) user namespace, kept so gdb can
  `attach` to other workload processes under `ptrace_scope=1`. It confers nothing over the
  init or anything outside the sandbox.
- **Resource limits:** none by default. A project can set them (§7); enforced via a cgroup
  when the system delegates one, else rlimits.

### 2.4 tursi never runs inside its own sandbox

The agent cannot start tursi — no nested agent loops, no reaching `__sandbox` or
`--no-sandbox` from inside. Three layers:

1. **No binary.** The running binary's path (`current_exe()`) and every `tursi` on `PATH`
   (`~/.cargo/bin/tursi`, `/usr/local/bin/tursi`, …) are masked with an empty,
   non-executable file, even where their directory is mounted.
2. **No `/proc/1/exe`.** A process's executable stays launchable through
   `/proc/<pid>/exe` even when its path is hidden, and the init *is* tursi. The init
   marks itself non-dumpable (and its spawns stay so until they exec), which locks its
   `/proc` entries against anything lacking `CAP_SYS_PTRACE` over it — and the workload's
   ptrace capability is confined to the inner namespace (§2.1). The harness itself is
   outside the pid namespace and has no `/proc` entry inside.
3. **No model access.** Even a copy built from source (working on tursi's own repo) cannot
   run an agent: model API hosts are not on the network allowlist (§4.2), `~/.tursi`
   (config, secrets) is not mounted, and secret-shaped environment variables are scrubbed.
   It could only reach a model if the user approved full network for that call.

## 3. Filesystem view

An **allowlist**: anything not listed does not exist inside the sandbox. No secret
deny-list to maintain — `~/.ssh`, `~/.aws`, `~/.tursi`, browser profiles, other projects
are absent because they are never mounted.

| Access | Paths |
|---|---|
| **read-write** | the project directory, at its real path; `<project>/.tursi/worktrees/` (the agent's git worktrees, §5.2) and `.tursi/profiles/` (profiler artifacts); `/tmp` — host `/tmp/tursi-<uid>/<session>/tmp`, wiped at session end; package caches — `~/.cargo/registry`, `~/.cargo/git`, `~/.npm`, `~/.cache/pip`, `~/.cache/uv`, `~/go/pkg/mod`; `/run/tursi/io` — step output files the harness reads |
| **read-only** | `/usr /bin /sbin /lib /lib32 /lib64 /opt /etc /sys`; toolchains — `~/.rustup`, `~/.cargo/bin`, `~/.local/bin`, `~/.local/lib`, `~/.pyenv`, `~/.nvm`, `~/go/bin`; their config — `~/.cargo/config.toml`, `~/.gitconfig`, `~/.config/git`; `<project>/.tursi` (except `worktrees/`); `.git/hooks` and `.git/config` when the project has a `.git` (§5.4); custom tool scripts (`~/.tursi/tools/bin` only); extra paths from config |
| **synthetic** | `$HOME` — an empty, writable tmpfs under the listed home paths, discarded at session end; `/proc` (sandbox processes only); minimal `/dev` (`null zero full random urandom tty pts shm`); `/run/tursi/proxy` — the network gate's socket, read-only (§4.1) |

Notes:

- The model never sees the `/tmp` indirection: inside, `/tmp` is `/tmp`.
- **`$HOME` is throwaway.** Tools may write dotfiles and caches there (`~/.cache/...`,
  shell history); nothing of the real home shows and nothing persists except through the
  listed caches. Everything outside the listed paths is read-only, so nothing new can be
  created at `/`.
- **Accepted risk — caches:** package caches are writable by every process in the sandbox,
  so sandboxed code could plant files in `~/.npm` or `~/.cargo/registry`. (Package managers
  run project code — `build.rs`, postinstall, `setup.py` — so "only the package manager
  writes" cannot be enforced by mounts alone.)
- **Accepted risk — project secrets:** `.env` and similar files inside the project are
  readable. A prompt rule tells the model to show names, never values (e.g. read a `.env`
  as `KEY=<redacted>`), and the existing redaction (`*_KEY=`, `Bearer`, `sk-…`) is applied
  to tool output as a backstop.

## 4. Network

### 4.1 Mechanism

The sandbox's network namespace has loopback only. `HTTPS_PROXY`/`HTTP_PROXY` point at a
loopback port forwarded (by the init) to a Unix socket mounted from the harness, where an
HTTP CONNECT proxy decides every connection by hostname. IP literals and non-standard ports
are refused. DNS is not needed inside: clients hand the hostname to the proxy. Tools that
ignore proxy variables (git over ssh, raw sockets) have no network.

### 4.2 Allowed by default

Package registries: `crates.io`, `index.crates.io`, `static.crates.io`,
`registry.npmjs.org`, `registry.yarnpkg.com`, `pypi.org`, `files.pythonhosted.org`,
`proxy.golang.org`, `sum.golang.org`. Plus entries from config (§7), each `host` (ports 80
and 443), `.suffix` (the domain and every subdomain), or `host:port`.

### 4.3 Anything else: asked, per call

- A step that needs more declares it: `execute_command` steps take `network: "full"`. The
  harness asks the user before the call runs ("allow full network for: <steps>"); approval
  lasts for **that one call** only.
- A call without the flag that hits a blocked host gets a refusal from the proxy (HTTP 403);
  the tool result ends with `[network] blocked: host:port, …` and tells the model to rerun
  with `network: "full"` if the access is needed.
- AFK/headless: the request is refused and the model is told, so it can work around it.

## 5. Projects and git

### 5.1 The project

- **Root:** the directory tursi starts in — unless an ancestor already has a `.tursi/`
  directory, in which case that ancestor is the project. No git repository is required.
- **State** lives in `<project>/.tursi/` (sessions, logs, checkpoints, project config). If
  the project happens to be a git repo, tursi adds `.tursi/` to `.git/info/exclude` so
  `git status` stays clean — the only thing tursi ever does to a repository, and it is a
  plain file append, not a git command.

### 5.2 In a git repo: the agent works in its own worktree (prompt rule)

The prompt tells the model, when the project is a git repo and the task changes files:

- Create a worktree under `.tursi/worktrees/<name>` on a new branch, and do the work there.
  Reuse it for follow-up tasks in the same session.
- Bring in the ignored environment the work needs (`.venv`, `node_modules`, build caches)
  from the main checkout — symlink or copy.
- Commit freely on its own branch. Never touch the user's branch, index, or stash.
- Report the branch name in its summary. Merge into the user's branch only when asked —
  and then resolve any conflicts, asking about unclear ones (§1.2).
- If the user's checkout has uncommitted changes the task depends on, ask whether to bring
  them along.
- Call out changes to files that run code on the user's machine — `.husky/`, `.githooks/`,
  `.envrc`, `.vscode/tasks.json`, `.pre-commit-config.yaml` — in its report.

Nothing enforces this: the main checkout is writable. The rule makes the agent's work
reviewable as a branch; the sandbox still guarantees nothing escapes the project.

### 5.3 Without git: in place

The agent edits the project directly. Protection is the edit-tool checkpoints (`:rewind`,
SPEC §3.3) and the must-ask rule for destructive steps (§1.2). No snapshots.

### 5.4 `.git/hooks` and `.git/config` are read-only

The agent can run any git command, but cannot plant a hook or a config entry (`core.pager`,
`core.fsmonitor`, aliases) that would execute on the user's machine the next time *they*
run git. Tracked hook directories (husky's `.husky/`) are project content and stay
writable; §5.2's call-out rule covers them.

## 6. No sandbox: benchmarks only

If namespaces are unavailable (inside docker/podman, hardened kernels), tursi refuses to
start and says why. `--no-sandbox` is the single escape, and it exists only for
benchmarks inside a disposable container:

- accepted **only together with `--task`** (headless) — the interactive TUI never runs
  unsandboxed;
- prints a warning on start; `--help` reads "benchmarks inside a disposable container only";
- network policy is the container's.

Headless runs (`--task`, sandboxed or not) tell the model to edit in place instead of using
a worktree (§5.2): a script or grader reads the project directory.

## 7. Configuration

`[permissions] allow/deny` is removed. New keys (global `~/.tursi/config.toml`, extended
per project in `.tursi/config.toml`):

```toml
[network]
allow = ["github.com"]            # added to the registry defaults (§4.2)

[sandbox]
read_only  = ["~/src/shared-lib"] # extra read-only mounts
read_write = []                   # extra writable mounts — use sparingly

[limits]                          # default: none (§2.3)
memory = "8G"
processes = 1024
```

## 8. Prompt changes (core.txt)

- **Rule 8** (git): replaced by §5.2 — in a git repo, work in your own worktree and branch;
  never touch the user's branch, index, or stash; merge only when asked. Headless sessions
  get "edit in place" instead (§6).
- **Rule 13** (`execute_command`): the `network: "full"` step flag and when to use it.
- **Rule 16** (rejections): only network requests can be refused now.
- **Rule 17** (`ask_user`): the must-ask list (§1.2) and the no-one-there behavior (§1.3),
  replacing "only for decisions genuinely yours to escalate".
- **New:** secret values never enter the context — show names, not values.

## 9. Implementation phases

1. **Sandbox core** — `tursi __sandbox` init, namespaces (init outer, workload nested),
   mount table, privilege drop, per-session lifetime, spawn protocol; self-execution
   blocks (§2.4); refuse-to-start and benchmark-only `--no-sandbox`; project root without
   git (start dir or nearest `.tursi`).
2. **Everything inside** — ripgrep, language servers, gdb (attach within the sandbox),
   rizin, profile, custom tools; file-tool path policy and `/tmp` translation.
3. **Network** — network namespace, proxy, registry defaults, `network: "full"` with
   per-call approval, blocked-host reporting.
4. **Removal, prompt, docs** — modes, patterns, approval overlays; prompt changes (§8);
   fold into SPEC.md and PROMPT.md.

## 10. Implementation notes

- **Worktrees need no special mounts.** A worktree's `.git` file points at
  `<project>/.git/worktrees/<name>`, a real path that is mounted writable; `git worktree
  add` writes refs and `.git/worktrees/`, never `.git/config` or hooks.
- **Spawning into a live sandbox:** language servers, gdb, and rizin are long-lived;
  the init spawns them on request and relays their stdio to the harness.
- **`.tursi/worktrees/` carve-out:** mounted writable on top of the read-only `.tursi`,
  so it is on the project's filesystem (copies of `.venv`/`node_modules` can be reflinks
  where supported).

## 11. Open questions

- **Untrusted project config.** A project can ship its own `.tursi/config.toml` (git ignores
  it locally, but a repository can still commit one, and a downloaded directory can contain
  one). It could add network domains or verify commands. Proposed: trust a project config
  only after the user approves it once (by content hash), like direnv's `allow`.
- **Worktree cleanup.** Agent-created worktrees accumulate in `.tursi/worktrees/`. The
  prompt could tell the agent to remove a worktree after its branch is merged; stale ones
  could be listed at session start.
- **Nested namespaces.** §2.4 stops tursi itself, but the workload can still create its own
  namespaces (a from-source tursi's sandbox, bubblewrap, Chromium's sandbox, rootless
  podman, tursi's own sandbox tests). Forbidding that (`max_user_namespaces=0` inside)
  would close the door completely but break those tools. Default: allowed — anything
  nested is still inside the wall.
