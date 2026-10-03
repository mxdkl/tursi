# Tursi: Architecture & Technical Specification

> Name: **tursi** (crate and binary; free on crates.io as of 2026-09-21).

A local, token-optimized autonomous coding agent written in **Rust** with an immediate-mode
**Ratatui** terminal interface and Vim-style modal keybindings.

> **Containment is specified in [PERMISSIONS.md](PERMISSIONS.md)** — the sandbox, the
> filesystem view, the network gate, and the project/git rules. Where this document and
> that one overlap, PERMISSIONS.md is the design of record.

## 0. Design Principles

1. **Edit in place, inside a wall.** The agent works directly in the project directory —
   the harness manages no worktrees and no merge-back — and everything it runs (commands,
   language servers, debuggers) runs inside a per-session namespace sandbox
   (PERMISSIONS.md §2). Safety comes from the sandbox, checkpoints, and staleness checks,
   never from command patterns or approval prompts. In a git repo the agent isolates its
   own work in a worktree by prompt rule (PERMISSIONS.md §5.2), not by harness mechanism.
2. **The model asks for what it needs.** Timeouts, output budgets, and read ranges are
   parameters the agent sets per call, not global constants the harness guesses.
3. **Failed iterations are the real token cost.** Context parsimony must never starve the
   model into a bad edit; truncation is always recoverable (full data one cheap tool call away).
4. **Nothing model-specific is hardcoded.** Model IDs and prices live in config and can
   change without a recompile.

## 1. Directory Layout & Unified Storage

A single dot-folder in the home directory holds all portable state. `tar ~/.tursi`
moves config, ledgers, and custom agent tools to another machine. Secrets are deliberately
excluded from the portable folder's main config.

```text
~/.tursi/
├── config.toml                 # Model, prices, budgets, [network]/[sandbox] (PERMISSIONS.md §7)
├── secrets.toml                # API keys only. chmod 0600. Values may be literal or "env:VAR_NAME"
├── stats/                      # Append-only JSONL ledgers partitioned by month
│   ├── 2026-08.jsonl
│   └── 2026-09.jsonl
└── tools/                      # Agent self-authored custom tools
    ├── registry.json           # Schema definitions dynamically loaded into context
    └── bin/                    # Scripts written by the agent for reuse

<target-project>/
└── .tursi/                   # Auto-added to .git/info/exclude on first run
    ├── config.toml             # Project overrides: verify/typecheck commands, [lsp]
    ├── checkpoints/            # Pre-edit file snapshots, per session (pruned on session end + age)
    ├── profiles/               # Flamegraphs and raw profiler artifacts from the profile tool
    ├── worktrees/              # The agent's own git worktrees (PERMISSIONS.md §5.2)
    ├── sessions/               # Journaled session state for crash recovery / resume
    └── debug.log               # Full, unpruned stdout/stderr tool logs (secret-redacted)
```

Notes:
- `secrets.toml` supports `key = "env:ANTHROPIC_API_KEY"` indirection so the file itself
  can be empty on machines that use environment variables or a secret manager.
- `debug.log` is written through a redaction filter (`*_KEY=`, `*_TOKEN=`, `Bearer …`,
  common secret shapes) before hitting disk.

## 2. Ratatui UI: Vim-Modal Interface

No vim crate. The modal layer is hand-rolled following the `newbbs` pattern
(`~/newbbs/src/ui/keys.rs`): a small `Mode` enum on the `App` struct, one key-event
dispatcher, and a deliberately tiny keymap — `:help` fits on one screen. Full vim
emulation is explicitly a non-goal.

### Structure (lifted from newbbs)

* `Mode { Normal, Insert, Command }` plus `pending: Option<char>` for two-key sequences
  (`gg`), and an `Overlay` enum for popups that capture all keys before mode dispatch.
* One `handle(app, key)` entry point: filter key-release events (Windows sends them),
  keep **Ctrl+C as an always-works quit in every mode** so the session can never be
  trapped, route to the open overlay first, then to the per-mode handler.
* Insert-mode editing is char-indexed with a `byte_index` helper so multi-byte input is
  safe; Ctrl+U clears the line, Ctrl+W deletes a word.
* Ergonomics preserved: Backspace on an empty `:` line drops back to Normal; Enter in
  Insert submits and returns to Normal so `j`/`k` work immediately; a failed `:` command
  reports on the status line and never tears down the session.
* Overlays in the harness: the **network prompt** (`y`/`n` + reason, PERMISSIONS.md §4.3),
  the **ask_user** question, `:help`, and `:sysinfo`.

### Mode Behaviors

* **Normal Mode (`Esc`):** Navigate the streaming transcript with `j`/`k`/`gg`/`G` and
  paging keys. The network prompt is answered here (`y` allow, `n` refuse with a reason).
  While a task is running, `Esc` interrupts it — the in-flight request or tool is cancelled and the
  prompt returns with the partial transcript intact (§5.2).
* **Insert Mode (`i` / `a`):** Focuses the bottom prompt bar to type instructions.
* **Command Mode (`:`):** Flat verb dispatch (`split_once` on whitespace, usage-string
  errors). Harness administration without talking to the LLM:
  * `:q` — quit (confirms if a task is running)
  * `:model <name>` — pin a model for the session; `:model` alone unpins
  * `:rewind [n]` — restore files from checkpoint(s), optionally rolling back the
    conversation to the same point
  * `:plan [task]` — start a skeleton-plan task (§5.5)
  * `:approve` — accept a skeleton and begin fill-in (§5.5)
  * `:afk` — toggle unattended mode: `ask_user` auto-resolves instead of blocking (§4.4)
  * `:sysinfo` — display the system card (§4.5)
  * `:log <pattern>` — grep `debug.log` from the UI

### TUI Layout

```text
┌─────────────────────────────────────────────────────────────────────────────┐
│ ~/my-project (feat/auth) │ deepseek-v4 │ Sess: $0.04 │ Month: $14.82        │
├─────────────────────────────────────────────────────────────────────────────┤
│ > [User]: Add a check for expired JWT tokens in the auth middleware.        │
│                                                                             │
│ [Agent]: Checked definition via `code_intel` (rust-analyzer).               │
│ [Agent]: Editing `src/middleware/auth.rs`                                   │
│                                                                             │
│  @@ -45,3 +45,5 @@ pub async fn validate(token: &str) -> Result<()> {       │
│       let claims = decode_token(token)?;                                    │
│  +    if claims.exp < Utc::now().timestamp() { return Err(AuthError); }     │
│                                                                             │
│ [Sandbox]: `cargo test` (timeout 120s, tail 40) -> 42 passed.               │
├─────────────────────────────────────────────────────────────────────────────┤
│ -- NORMAL --                                                                │
└─────────────────────────────────────────────────────────────────────────────┘
```

`AFK` and `PLAN` show as badges in the status bar when active.

## 3. Edit Model: In-Place, Checkpointed

### 3.1 The edit tool

Structured tool call, not in-text diff fences (fences are a fallback for models that
fumble tool JSON, behind a config flag):

* `edit(edits[…])` — each hunk's `old_string` must match the file content exactly and
  uniquely; the harness rejects ambiguous or missing matches with a precise error.
  Hunks targeting one file validate together against current content and apply as **one
  mutation** — one checkpoint, one diagnostics run. A failing hunk
  fails its whole file's transaction; other files' transactions stand independently.
* `read(reads[…])` — ranged, multiple files/ranges per call. Truncated output always
  ends with an explicit marker:
  `[truncated at line N of M — request a larger range if needed]`.
* After every successful edit, the harness runs `code_intel` diagnostics on the file and
  returns any new errors alongside the edit confirmation.

### 3.2 Staleness checks (optimistic locking)

The harness records a content hash of every file the agent reads. An `edit` against a
file whose on-disk hash no longer matches the last-read hash **fails** with
"file changed since last read — re-read before editing." This covers both user edits made
while the agent works and the agent's own concurrent tool calls. There is no merge logic
anywhere in the harness.

### 3.3 Checkpoints

Before any file mutation, the harness snapshots the file into
`.tursi/checkpoints/<session>/`. `:rewind` restores any earlier state without touching
git history. Checkpoints are pruned when a session is closed cleanly and by age.
The agent never runs `git commit` / `git push` unless the user explicitly asks.

### 3.4 No approval modes

There is no NORMAL/AUTO switch and no command allow/deny list: the sandbox is the
boundary (PERMISSIONS.md §1). Edits apply, commands run. The one thing that prompts is
network access beyond the package registries — a step that sets `network: "full"` asks
the user, and the grant covers that call only (PERMISSIONS.md §4.3). Unattended, it is
refused and the model is told.

**A refusal is steering, not a halt.** It reaches the model as an error tool result with
the user's optional one-line reason, and the loop continues so it can adapt.

## 4. Tool Surface

Every tool schema is paid for in tokens on every request, so the bar for inclusion is
high: eleven tools. Everything else goes through `execute_command` inside the sandbox.
Self-authored tools from `~/.tursi/tools/registry.json` are appended to this roster at
session start.

| Tool | Signature | Where it runs |
|---|---|---|
| `read` | `(reads[{file, offset?, limit?}])` | harness, through the sandbox's view (PERMISSIONS.md §2.2) |
| `write` | `(file, content)` | harness, through the view |
| `edit` | `(edits[{file, old_string, new_string, replace_all?}])` | harness, through the view |
| `search` | `(pattern, path?, glob?, max_results?)` | ripgrep inside the sandbox |
| `execute_command` | `(steps[{command, cwd?, env?, streams?, timeout_seconds?, tail_lines?, network?}], on_error?)` | inside; `network: "full"` prompts |
| `profile` | `(command, mode, runs?, baseline?)` | inside |
| `debug` | `(actions[…])` — stateful session | gdb inside; `attach` reaches sandbox processes only |
| `rizin` | `(open?, deep?, commands[], close?)` — stateful RE session | inside |
| `code_intel` | `(queries[{action, symbol?, file?, line?, col?}])` | language servers inside |
| `log_search` | `(pattern, context_lines?)` | harness (its own debug.log) |
| `ask_user` | `(question, options?)` | is itself a prompt |

**Minimal output contracts.** The transcript is append-only between compactions (§8),
so a tool result is not paid for once — it is paid for on every request that follows it
in the task. Every tool therefore returns a distillate designed to sit in context
without regret; the raw stream never enters the transcript. Shared rules:

1. **Success is one line** — `exit 0 in 2.3s (14 warnings)`,
   `applied at line 45 — diagnostics clean`. Never echo what the model already knows:
   its own edit text, the command it ran, the content it wrote.
2. **Failure returns the extraction, not the stream** — the error-aware regions (§6.3),
   sized by `tail_lines`.
3. **Everything is recoverable** — full output goes to `debug.log` keyed by an id;
   results end with a pointer (`[full: log#4821]`) and `log_search` pulls more.
   Minimization is a default, never a ceiling.
4. **Re-reads dedupe** — a `read` whose content hash matches an earlier in-context read
   returns `unchanged since turn N` instead of the bytes (re-sent in full if the
   earlier copy was compacted away).

The floor is §0 principle 3: a result minimized so hard the model needs a second tool
call to proceed costs a full round-trip — more than it saved. Essentials means enough
to act on. Source code being read for editing is irreducible payload: ranges and
dedupe are the only levers there, never summarization. Stop reports (§4.3) and profile
summaries (§4.2) are instances of these rules.

**Batch-first calls.** A round-trip bills the entire context, so every tool accepts a
list of operations where the operations are related or order matters: `execute_command`
takes steps in one shell, `read` takes multiple files/ranges, `code_intel` takes
multiple queries, `debug` takes an ordered action list (two breakpoints + `continue` in
one call), `edit` takes hunk lists with per-file atomicity (§3.1). One call, one result
block, one bill.

### 4.1 Tool notes

* **`read`** — ranged and batched (multiple files/ranges per call), line-numbered
  output (compiler errors and diagnostics reference line numbers, so the model
  correlates them reliably), explicit truncation marker, records the staleness hash
  (§3.2).
* **`write`** — creates files. Overwriting an existing file requires a prior `read`
  this session — the same staleness discipline as `edit`, so the model can never blow
  away a file it hasn't looked at.
* **`search`** — ripgrep-backed; structured `file:line:` results grouped by file, hard
  result cap with an "N more matches" line. A dedicated tool rather than shell `rg`
  because it removes shell-quoting errors and guarantees output caps. File finding
  (glob-only) is the `glob` parameter with no pattern.
* **`execute_command`** — takes a **steps array**, never a compound command: the
  chaining operators `&&`, `;`, `||`, `&`, and newlines are rejected with "use steps",
  so every step has its own exit code and its own output. Steps run sequentially in
  **one shell** (bash when installed), so `cd` and `export` persist across steps (never
  across calls — each call starts fresh at the project root); a step's `cwd`/`env`
  apply to that step alone. Pipes are legal — a pipeline is one logical command and the
  model's own minimization tool (`rg foo | head -5`) — and a failing stage upstream of
  `| tail` fails the step (PIPESTATUS). The exit code and timing always come back; per-step `streams`
  picks what accompanies them: `auto` (default — the command's stdout on success, which
  is empty for build/test-style commands and the point for `find`/`ls`/`grep`; the
  error-aware extraction across both streams on failure), `none`, `stdout`, `stderr`, or
  `both` (both output streams), all under `tail_lines` truncation.
  `on_error`: `stop` (default) or `continue`; skipped steps report `not run`. A step
  that needs network beyond the package registries sets `network: "full"`, which prompts
  the user once for the call (PERMISSIONS.md §4.3). Sandbox and limits in §6.
* **`code_intel`** — **symbol-first addressing**:
  `code_intel("references", symbol="validate_token")`. The harness resolves names via
  LSP `workspace/symbol`; `file`/`line`/`col` is the fallback for ambiguous symbols.
  Actions: `definition`, `hover`, `references`, `diagnostics`. Models produce names
  reliably and exact columns unreliably; the addressing mode matches that.
* **`log_search`** — grep over the current session's full `debug.log`; the recovery
  path that makes aggressive `tail_lines` defaults safe.

### 4.2 `profile` — performance measurement

One tool, five modes, aimed at low-level work. The harness owns the profiler flag soup
and compresses the output; raw artifacts go to `.tursi/profiles/`.

| Mode | Backend | Returns |
|---|---|---|
| `time` | hyperfine-style runner | warmup + N runs; mean ± σ, min/max; with `baseline`, an A/B delta ("1.42x faster") |
| `counters` | `perf stat` | cycles, instructions, IPC, branch/cache miss rates, page faults, context switches |
| `hotspots` | `perf record` → collapsed stacks | top ~15 functions by self-time inline; flamegraph SVG written to `.tursi/profiles/`, path returned |
| `allocs` | dhat / heaptrack | allocation count and bytes, peak RSS, top allocation sites |
| `syscalls` | `strace -c` | syscall summary table |

Profiled commands run in the same sandbox as `execute_command`. Startup probes
`kernel.perf_event_paranoid` and which backends are installed; the result is reported
in the system card, so the model knows what it can call before trying.

### 4.3 `debug` — interactive debugger sessions

The one stateful tool: it drives a persistent debugger session over **DAP** (Debug
Adapter Protocol) — the same facade pattern as `code_intel`. Default backend is GDB's
native DAP interpreter (`gdb -i dap`, GDB ≥ 14), covering C, C++, and Rust; other
adapters (`debugpy`, `delve`) plug in per language. One live session at a time (v1);
sessions idle-timeout and are killed at task end.

Each call takes an **ordered list of actions**, stop-on-first-error — set two
breakpoints and `continue` in one round-trip.

Actions:

* **Session** — `launch(program, args?, stop_at?)`, `attach(pid)`,
  `open_core(program, core)` for post-mortem analysis, `quit`.
* **Breakpoints** — `break(location, condition?)` where location is a function name,
  `file:line`, or `*address`; `watch(expr_or_addr)` for hardware watchpoints.
* **Execution** — `continue`, `step`, `next`, `finish`; each takes a `timeout_seconds`
  cap like `execute_command`, so a runaway `continue` cannot hang the loop.
* **Inspection** — `stack(max_frames?)`, `locals(frame?)`, `eval(expr, frame?)`,
  `registers`, `read_memory(addr, len)` (bounded hex+ASCII window),
  `dump_memory(addr, len, path)` (raw range to a file — JIT'd/self-modifying
  code that isn't in the on-disk binary), `disassemble(at, count?)` (via the
  DAP disassemble request, so it works on raw addresses in stripped binaries).
  A `break` on a bare `0xADDR` sets an instruction breakpoint — gdb's
  function/line breakpoints silently never fire on a raw address in a
  stripped binary.
* **Escape hatch** — `command(text)`: a raw gdb console command via DAP's repl-context
  evaluate, for the long tail (`info proc mappings`, catchpoints, …).

**Stop reports:** any resuming action returns a compact report — stop reason
(breakpoint / watchpoint / signal / exit), thread, location, top 5 frames, and watched
values — never the raw debugger transcript, which goes to `debug.log` as usual.

**Reverse debugging (optional backend):** when `rr` is installed and the CPU and
`perf_event_paranoid` allow it, `launch(record=true)` records the run; the session then
replays deterministically with `rcontinue` / `rstep` reverse execution — set a
watchpoint on a corrupted address and reverse-continue to the instruction that wrote
it. Probed at startup and reported in the system card.

**Sandbox:** gdb and the debuggee run inside the session sandbox. `attach` reaches only
processes inside it — the agent's own — so it needs no prompt (PERMISSIONS.md §2.3).
`kernel.yama.ptrace_scope` joins the startup probes.

### 4.4 `ask_user` & AFK mode

`ask_user` renders as a TUI overlay and rings the terminal bell. It blocks even though
nothing else prompts: autonomy is permission to act, not authority to decide —
PERMISSIONS.md §1.2 lists what the model must always ask. `:afk` toggles unattended
mode: `ask_user` then
returns immediately with "user unavailable — proceed on best judgment and log the
assumption," and each question/assumption pair is highlighted in the transcript for
review when the user returns.

### 4.5 The system card

At session start the harness composes a compact (~200 token) hardware/OS block into the
system prompt, so low-level decisions (SIMD width, thread counts, cache blocking) never
cost a discovery turn:

* **CPU** — model, arch, cores/threads (P/E split if hybrid), boost clock, L1d/L2/L3
  sizes, NUMA node count
* **ISA flags** — curated perf-relevant subset only (`avx2`, `avx512*`, `fma`, `bmi2`,
  `sse4.2`, `aes`, `sha_ni`, `amx*`, …), never the full `/proc/cpuinfo` flag soup
* **Memory** — total RAM, swap
* **GPU(s)** — model, VRAM, driver (`nvidia-smi`, `lspci` fallback)
* **OS** — distro, kernel, libc; filesystem of the project directory
* **Toolchains** — rustc/cargo, cc/clang, python versions
* **Capabilities** — sandbox status, `perf_event_paranoid`, `ptrace_scope`, available
  `profile` backends, debugger backends (gdb version / DAP support, `rr` availability)

Sources: `lscpu`, `/proc/meminfo`, `/etc/os-release`, `uname`, `nvidia-smi`. Composed
once per session; `:sysinfo` displays it in the TUI.

## 5. Agent Workflow

### 5.1 Sessions and tasks

A **session** is one TUI process attached to one project: one conversation, one stats
trail, one live `debug` slot. A **task** is one user instruction plus the loop
iterations until control returns to the user. Tasks share the conversation, so context
carries forward.

### 5.2 The core loop

Single loop, model-driven — no enforced explore/plan/execute phasing (plan mode, §5.5,
is opt-in). One iteration:

1. Inject any queued steering messages — the user can type while the agent runs;
   messages land at the top of the next iteration.
2. Budget check: context fraction → compaction (§8); cost caps (§9) → warn / halt.
3. Call the model, streaming tokens and tool-call deltas to the transcript.
4. **No tool calls in the response → the agent's turn ends** (§5.3).
5. Execute tool calls: read-only tools (`read`, `search`, `code_intel`, `log_search`)
   run **in parallel**; mutating tools run serially. The executor is async-first from
   day one.
6. Append results; loop. `max_turns_per_task` (config; 0 = unlimited, the default) is an
   optional ceiling — budgets and Esc are the real bounds.

`Esc` in Normal mode while a task runs **interrupts**: the in-flight request or tool is
cancelled and the prompt returns with the partial transcript intact (Ctrl+C remains
hard-quit). Outstanding tool calls receive synthesized `interrupted by user` error
results so the transcript stays well-formed for the next request; crash-resume performs
the same repair — a crash is an involuntary interrupt. An interrupt followed by a typed
message therefore reads to the model as firm steering.

### 5.3 Turn end & the verification gate

* **Attended:** control returns to the user; the closing text is a report, a question,
  or a done-claim, and the user judges it. There is deliberately no `task_complete`
  tool — models chronically misuse explicit completion signals, and plain text keeps
  interactive use unceremonious.
* **AFK:** the harness runs the project's **verify commands** (from the project
  `.tursi/config.toml`, e.g. `verify = ["cargo check", "cargo test"]`). Green → task
  complete, summary in the transcript. Red → an error-aware excerpt is injected as a
  harness `[verify]` user-role message and the loop continues, bounded by
  `max_turns_per_task`. (User-role because tool results must answer a pending tool
  call id, and after a no-tool-call turn there is none; steering uses the same
  vehicle.)

### 5.4 One model

A session runs one configured model (`model` in config); `fallbacks` lists same-provider
models tried when the API keeps failing. There is no escalation, tiering, or handoff —
provider-level errors retry with backoff (except 4xx the provider will reject again),
then fail over. `:model <name>` pins a model for the session.

### 5.5 Plan mode: skeleton plans

For large tasks, entered with `:plan` (the agent may also propose it). The plan
artifact is **not prose — it is a compilable skeleton**:

* Every new file is created, with real module wiring (`mod` declarations, imports).
* Every function is stubbed with its real signature — name, typed parameters, return
  type — and a body of the language's stub marker (`todo!()`,
  `raise NotImplementedError`, `throw new Error("TODO")`).
* **Orchestration bodies are filled in with their call sequences** (`main` calls
  `config::load`, then `ui::init`, …) so control and data flow are visible rather than
  described.
* The data model is concrete: structs, enums, and type declarations with fields.
* Optionally, stubbed test functions whose names state the acceptance criteria
  (`fn rejects_expired_token()`).

**The plan gate is the typechecker.** A skeleton must pass `cargo check` (or
`tsc --noEmit`, `mypy`, …) before it is presented: a skeleton that typechecks is an
architecture proven coherent — every call resolves, every type lines up. This is the
point of skeleton plans over English ones: the plan itself is machine-verified, and
design flaws that only become obvious when you see the real shape surface before any
body is written.

Mechanics:

* Plan mode is entered with `:plan`. The typecheck gate always applies; skeleton
  approval is `:approve` when attended and auto-granted after the gate in AFK.
* Status bar shows **PLAN**. Review is holistic: browse the skeleton, edit it directly,
  or steer in chat.
* `:approve` exits plan mode into **fill-in**; rejecting is one `:rewind` of the whole
  skeleton.
* Remaining stub markers are the fill-in work queue and the progress meter (status
  bar: `stubs 17/23`); the harness counts them with `search`.

### 5.6 Session state machine

```
IDLE ──user msg──► RUNNING ──network prompt──► AWAITING_APPROVAL ──┐
 ▲                  │  ▲  ◄────────────────────────────────────────┘
 │                  │  └──ask_user──► AWAITING_USER ──answer──┐
 │                  │  ◄──────────────────────────────────────┘
 │            no-tool-call turn
 │                  │
 │            [AFK] ▼
 │              VERIFYING ──red──► RUNNING
 └────green / attended──┘
```

Every transition is journaled to `.tursi/sessions/<id>.jsonl` (§9); crash-resume
replays the journal to the last stable state and continues on the persisted transcript.

### 5.7 Subagent readiness (deferred feature, binding constraints)

Subagents/swarms are **not** in v1, but v1 is built so they bolt on. When they arrive,
a subagent is **a tool call** — `agent(role, brief)` → a minimal report — and the
parent's transcript sees only the distillate: context isolation is the minimal output
contract applied to a bigger tool. To keep that true, v1 must obey:

1. **`AgentLoop` is a composable unit** — `(brief, toolset, model, budget)` in,
   report out; no process-global state. Spawning is calling the constructor twice.
2. **Ids everywhere** — every UiEvent, journal line, ledger entry, and `log#` key
   carries an agent id (always the root agent in v1). Fields now, not a migration later.
3. **The `is_read_only` bit is the capability mask** — the per-tool property that §5.2
   parallelism already needs doubles as the subagent toolset mask. First-generation
   subagents (explore, review, analysis) are read-only, so the working tree keeps
   exactly one writer and the in-place edit model survives. Write-capable swarms
   require worktrees and stay out of scope (§10).
4. **Children never prompt** — no `ask_user`, no network prompts from subagents;
   anything unresolvable goes in the report and the parent or user decides. Children
   are structurally AFK + read-only, so no approval plumbing crosses agent boundaries.
5. **Spawn brief** — a compact task/state summary handed to the child; the same shape
   compaction (§8.3) will eventually produce.

Anticipated config shape: `[agents.<role>]` tables (system prompt, tool mask, model,
`max_turns`). Deliberately undesigned: scheduling, inter-agent messaging, shared state,
parallel mutation.

## 6. Execution, Sandboxing & Adaptive Limits

### 6.1 Tool schema

```json
{
  "name": "execute_command",
  "description": "Run commands in the sandbox. Steps run sequentially in ONE shell: cd and exports persist across steps (not across calls). Chaining operators (&&, ;, ||, &) are rejected — use separate steps. Pipes are allowed.",
  "parameters": {
    "type": "object",
    "properties": {
      "steps": {
        "type": "array",
        "items": {
          "type": "object",
          "properties": {
            "command": { "type": "string" },
            "streams": {
              "enum": ["auto", "none", "stdout", "stderr", "both"],
              "description": "Which output streams to include — exit code and timing are always reported. auto: stdout on success (empty for builds, the point for find/ls/grep), error extraction on failure. both: stdout and stderr.",
              "default": "auto"
            },
            "timeout_seconds": { "type": "integer", "default": 30 },
            "tail_lines": { "type": "integer", "default": 20 },
            "network": { "enum": ["full"], "description": "Needs hosts beyond the package registries; the user is asked, for this call only." }
          },
          "required": ["command"]
        }
      },
      "on_error": { "enum": ["stop", "continue"], "default": "stop" }
    },
    "required": ["steps"]
  }
}
```

Result format, one line per step unless a capture demands more:

```text
1 ✓ cargo build          (4.1s)
2 ✗ cargo test           exit 101 (2.3s) — test auth::expired ... FAILED [full: log#4821]
3 – cargo bench          not run (on_error=stop)
```

The maximum permitted `timeout_seconds` is a config value (default cap 600) — large cold
builds can legitimately need more.

### 6.2 Sandboxing

Specified in PERMISSIONS.md §2–4; in brief:

1. **One sandbox per session**, built by tursi re-executing itself as the sandbox's
   PID 1 (`tursi __sandbox`): user, mount, pid, net, ipc, and uts namespaces, no external
   helper. Everything that executes runs inside — shell steps, ripgrep, language servers,
   gdb, rizin, profilers, custom tools. The harness, its API key, and `~/.tursi` stay
   outside; tursi's own binary is masked inside (§2.4 there).
2. **Filesystem allowlist:** the project (read-write), a private `/tmp`, package caches,
   system directories and toolchains (read-only); `.git/hooks`, `.git/config`, and
   `.tursi` read-only. Anything unlisted — the rest of `$HOME`, `~/.ssh`, other
   projects — does not exist inside. The file tools enforce the same view.
3. **Network:** loopback only, plus an HTTP proxy the harness runs that admits package
   registries (and `[network] allow` from config); `network: "full"` on a step asks the
   user for that call.
4. **Fail closed:** no namespaces, no session. `--no-sandbox` exists only with `--task`,
   for benchmarks inside a disposable container.
5. **Tokio-enforced timeouts:** per-step `timeout_seconds` wraps the step future; on
   expiry the step's process group is SIGKILLed and remaining steps report `not run`.
6. **One shell per call:** steps execute sequentially inside a single shell in the
   sandbox, fed one at a time with sentinel markers that capture per-step exit code,
   stdout, and stderr separately. Environment changes (`cd`, `export`) persist across
   steps and die with the call.

### 6.3 Output handling

* Returned output is **error-aware**: the harness scans for compiler/test failure markers
  (`error[`, `FAILED`, `panicked at`, `Traceback`) and prioritizes those regions inside
  the `tail_lines` budget, rather than blind head/tail clipping.
* Full output always goes to `.tursi/debug.log` (redacted).
* A `log_search(pattern, context_lines?)` tool lets the agent grep the full log, so a bad
  truncation guess costs one cheap tool call, not a failed iteration.

## 7. Model

No model IDs in code. `config.toml` names the model and the prices used for cost
accounting:

```toml
model = "deepseek/deepseek-flash"
fallbacks = ["deepseek/deepseek-v4-pro"]   # same provider; tried when the API keeps failing
context_window = 131072

[prices."deepseek/deepseek-flash"]
input = 0.30    # $/M tokens
output = 1.20
cached_input = 0.006
```

Any OpenAI-compatible provider works by prefix (`deepseek/`, `kimi/`, `qwen/`,
`openrouter/`); credentials come from `<PREFIX>_API_KEY` or `~/.tursi/secrets.toml`.
There is no router: no tiers, no escalation, no summarized handoff (§5.4). `:model <name>`
pins a model for the session.

## 8. Code Intelligence & Context Management

1. **Single `code_intel` tool** exposing `definition`, `hover`, `references`,
   `diagnostics` with symbol-first addressing (§4.1); the harness routes to the correct
   local language server (`rust-analyzer`, `pyright`, `jdtls`, …).
   **Environment-correct servers:** resolution ladder per language — project
   `[lsp]` config override → the project's own environment (Python: `.venv`
   detected, `VIRTUAL_ENV`/`PATH` set at spawn AND the interpreter passed via
   workspace configuration; TS: `node_modules/.bin` + workspace typescript;
   clangd: `--compile-commands-dir`; rust-analyzer/gopls: natively per-project
   via rustup shims / go.mod) → PATH as last resort. Never `uvx`-style
   isolated envs (they can't see project packages) and never `uv run` for the
   spawn itself (it syncs as a side effect). One server per (project,
   language), owned by the per-project Toolbox — no global daemon to
   contaminate. Resolved command + interpreter are logged at spawn and shown
   in `:sysinfo`.
2. **Diagnostics settle strategy:** LSP diagnostics are asynchronous and lag edits.
   The harness waits on the server's progress notifications before reading diagnostics,
   and treats the compiler (`cargo check`, `tsc`, …) as the authority when they disagree.
3. **Context compaction:** when the conversation approaches a configured fraction of the
   context window, the harness summarizes the oldest turns (preserving the task statement,
   key decisions, and current file state) and continues on the compacted transcript.
   Between compactions the transcript is **append-only** — any rewrite of history
   invalidates the provider's cache prefix, so nothing is collapsed incrementally. A
   compaction event, already paying the cache bust, does everything at once: summarize
   the oldest turns and collapse mid-age tool outputs (recoverable from `debug.log`).
   Minimal output contracts (§4) are what keep compaction rare.
4. **Prompt assembly:** the request prefix is, in order: core system prompt (canonical
   text in `PROMPT.md`) → system card (§4.5) → project block → tool schemas in a stable
   order — byte-stable within a session. Anything that can change mid-session
   (plan mode, AFK) never touches the prefix: mode boundaries are
   injected as user-role messages (formats in `PROMPT.md`), so providers with
   automatic prefix caching keep their cache across every request. The prompt teaches
   only what schemas cannot: harness semantics (turn-end contract, staleness,
   rejection-as-steering) and behavioral rules — target ~600 tokens.

## 9. Sessions, Stats & Cost Accounting

* **Session journal:** every session appends state transitions to
  `.tursi/sessions/<id>.jsonl`. On startup after a crash, the harness offers resume;
  checkpoints from the interrupted session remain available to `:rewind`.
* **Stats ledger:** each API call appends `{ts, session, agent, task, model, in_tokens,
  cached_tokens, out_tokens, cost}` to the monthly JSONL in `~/.tursi/stats/` (`agent`
  is the root agent until subagents exist — §5.7). The status-bar session
  and monthly figures are computed from these ledgers using the config price table.
* **Budgets:** optional soft caps in config (`session_usd`, `month_usd`) — the harness
  warns at 80% and requires explicit confirmation to continue past 100%.

## 10. Out of Scope (v1)

* Windows/macOS sandboxing (Linux-only; macOS would need Seatbelt, Windows has no
  namespace equivalent).
* Parallel multi-task execution. The in-place edit model assumes one active task per
  project.
* Subagents/swarms — deferred, but §5.7's constraints bind v1 so they bolt on later.
  Read-only subagents fit the in-place model; write-capable swarms wait for worktrees.

## 11. Benchmarking

The efficiency thesis is falsifiable: tursi is benchmarked against other harnesses
(Claude Code headless, aider, OpenCode, and a naive-loop control) per the protocol in
`BENCH.md`. Two modes — same-model harness-efficiency runs, and full-system runs with
each harness in its default configuration. Headline metric: **$/solved task**, measured at a shared metering
proxy rather than any harness's self-reporting; suites are SWE-bench Verified and
Terminal-Bench subsets for comparability plus a custom post-cutoff systems suite
(Rust/C: races, memory corruption, perf targets) that exercises `debug`, `profile`,
and `code_intel` where public benches cannot. Grading is external with held-out tests
restored after the harness exits.
* MCP interop. The self-authored `tools/` registry covers extensibility for v1.
