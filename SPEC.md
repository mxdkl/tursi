# Tursi: Architecture & Technical Specification

> Name: **tursi** (crate and binary; free on crates.io as of 2026-09-21).

A local, token-optimized autonomous coding agent written in **Rust** with an immediate-mode
**Ratatui** terminal interface: a small chat beside a block of braille where each working
agent lights one colored dot.

> **Containment is specified in [PERMISSIONS.md](PERMISSIONS.md)** — the sandbox, the
> filesystem view, the network gate, and the project/git rules. Where this document and
> that one overlap, PERMISSIONS.md is the design of record.

## 0. Design Principles

1. **Edit in place, inside a wall.** The agent works directly in the project directory —
   the harness manages no worktrees and no merge-back — and everything it runs (commands,
   language servers, debuggers) runs inside a per-session namespace sandbox
   (PERMISSIONS.md §2). Safety comes from the sandbox, recorded changes, and staleness checks,
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
├── stats/                      # Append-only JSONL cost records partitioned by month
│   ├── 2026-08.jsonl
│   └── 2026-09.jsonl
├── decisions.jsonl             # Every decision-model call (gates, routing, QA)
├── deals/                      # DEALS station catalog and learned outcomes (§5.8)
└── tools/                      # Agent self-authored custom tools
    ├── registry.json           # Schema definitions dynamically loaded into context
    └── bin/                    # Scripts written by the agent for reuse

<target-project>/
└── .tursi/                   # Auto-added to .git/info/exclude on first run
    ├── config.toml             # Project overrides: verify/typecheck commands, [lsp]
    ├── ledger.jsonl            # Everything that happened here, append-only (below)
    ├── blobs/                  # Content-addressed payloads the ledger references
    ├── profiles/               # Flamegraphs and raw profiler artifacts from the profile tool
    ├── worktrees/              # The agent's own git worktrees (PERMISSIONS.md §5.2)
    ├── deals/memory/           # DEALS station memories (§5.8)
    └── lsp-<lang>.log          # Language-server stderr
```

**The ledger.** One file per project records everything that happens in it, one JSON
event per line: `{ts, session, agent, kind, …}`, where `agent` is 0 for the session's root and N
for `agent-N`. Kinds:
- `session` — opened, resumed, each state transition (§5.6), closed.
- `message` — one message appended to an agent's conversation; `snapshot` — the whole
  conversation, written only when history is rewritten (compaction, repair). An
  agent's conversation is its latest snapshot plus the messages after it: that is what
  `--resume`, follow-ups to `agent-N`, and the resumed UI replay.
- `goal` — `/goal` set or cleared.
- `output` — a tool's full output, redacted, inline when small, else a blob.
- `file` — a file change: path, content hash before and after, the agent, and how:
  `edit`/`write`, `shell` (with the command, and `overlap` for other agents whose
  commands ran at the same time), or `external`; plus how the new version is stored
  — usually a `diff` from the old one (§3.3).
- `trace` — the harness's own diagnostics (tracing, info and up).

An event's id is its byte offset — `log#N` in tool results — stable because the file
only grows. Payloads that would bloat it live in `blobs/`, named by their blake3 hash
and written once: tool output over 16 KB, diffs over 16 KB, and the whole file
versions §3.3 keeps. Appends from several tursi processes on one
project are serialized by an OFD lock the harness holds for the single write.
`log_search` and `/log` read the ledger; `.tursi/` is read-only inside the sandbox, so
nothing an agent runs can edit its own record.

Notes:
- `secrets.toml` supports `key = "env:ANTHROPIC_API_KEY"` indirection so the file itself
  can be empty on machines that use environment variables or a secret manager.
- Tool output is written through a redaction filter (`*_KEY=`, `*_TOKEN=`, `Bearer …`,
  common secret shapes) before it reaches the ledger.

## 2. Ratatui UI: Chat and Agent Tiles

No modes and no vim keys. Typing always goes to the input; the screen shows the
conversation, and the work happens in tiles.

### What is on screen

* **Header** — project, model, provider balance (§9), context gauge, and badges for
  PLAN, AFK, armed monitors, an active goal, and plan stub progress.
* **Chat** — your messages (`❯`), the replies (`●`: with DEALS on, the report of the pool
  agent working on your message, §5.7), one-line notes when a report wakes the root agent
  (`⚡ agent-2 reader … finished`), goal verdicts, and a `✻` footer per task. **Tool calls
  never appear here**: agents' calls light their dots, not the chat. Below the chat: the input box (titled
  "steer the running task" while one runs) and one status line — the working line
  (`⋯ working 1m 12s · 9 steps · 2 agents working · Esc interrupts`), a message, or key
  hints.
* **Agent block** (`ui/agents.rs`) — a pane beside the chat in a dim frame titled "agents",
  with the legend on its bottom edge. Unlit dots are
  invisible; every working subagent **lights one braille dot**, colored by kind (reader
  blue, writer green) and twinkling softly. Agents fill the block in
  reading order, from the top-left cell rightward and then down. Agents of the same kind
  share a cell, up to its eight dots; a different kind starts the next free cell, since
  a terminal colors whole cells. A dot keeps its place for the agent's whole life, fades
  over 1.5 s when the agent finishes (held until then), and is reused by the next agent
  of a fitting kind; a follow-up relights its old dot when it is still free. The legend
  counts the working agents by kind. Nothing else about subagents is shown.
* **Layout** — the block is always there, so the chat never resizes: at 110+ columns the
  chat (with the header above it) takes three quarters of the width and the block the
  last quarter, running the full height from the top line; narrower, the block is a
  strip across the top quarter under the header. A 200 ms tick drives the twinkle.

### Keys

* **Enter** sends; while a task runs it steers instead (§5.2). **Esc** interrupts a
  running task, otherwise clears the line. **Up/Down** recall earlier inputs.
* Editing: Ctrl+U clear, Ctrl+W delete word, Ctrl+A/E start/end, Ctrl+K kill to end;
  char-indexed with a `byte_index` helper so multi-byte input is safe.
* **PgUp/PgDn** scroll the chat (bottom-anchored), Ctrl+Home/End jump.
* **Ctrl+C** clears a half-typed line, otherwise quits — in every state, overlays
  included, so the session can never be trapped. Ctrl+D on an empty line quits.
* Overlays capture keys first: the **network prompt** (`y`/`n` + reason,
  PERMISSIONS.md §4.3), the **ask_user** question (1–9 or typed), `/help`, `/sysinfo`.

### Slash commands

A line whose first word is a known verb runs it; anything else starting with `/` (a
path) is sent as a message. Failures report on the status line, never tear down.

* `/quit` (`/q`, `/exit`), `/help`, `/sysinfo`
* `/model <name>` — pin a model for the session; `/model` alone unpins
* `/plan [task]` — start a skeleton-plan task (§5.5); `/approve` begins fill-in
* `/afk` — toggle unattended mode: `ask_user` auto-resolves instead of blocking (§4.4)
* `/goal [condition|clear]` — §5.3; `/monitors`, `/monitor stop <id>`
* `/log <pattern>` — the matching ledger events, one line each, into the chat

## 3. Edit Model: In-Place, Recorded

### 3.1 The edit tool

Structured tool call, not in-text diff fences (fences are a fallback for models that
fumble tool JSON, behind a config flag):

* `edit(edits[…])` — each hunk's `old_string` must match the file content exactly and
  uniquely; the harness rejects ambiguous or missing matches with a precise error.
  Hunks targeting one file validate together against current content and apply as **one
  mutation** — one recorded change, one diagnostics run. A failing hunk
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

### 3.3 Recorded changes

Every change to a project file is a `file` event in the ledger (§1): content hash
before and after (contents in the blob store) and who made it. Code: `src/changes.rs`.

* `edit` and `write` record their own change exactly.
* Shell commands are caught by comparing the project tree before and after each
  command, jj-style: stat fields (mtime, ctime, size, inode) first, and a file is
  re-read and hashed only when they moved. A change seen across a command is that
  agent's, with the command; if other agents' commands ran at the same time they are
  listed in `overlap`. A change seen between commands with nothing running is
  `external` (your editor, a background job).
* Tracked: what `git ls-files -co --exclude-standard` lists, or without git a walk that
  skips VCS directories, `.tursi` and build output; files up to 1 MiB; at most 50 000
  files, past which shell capture switches off with a warning.
* Storage. Every version is named by its blake3 hash and kept one of three ways:
  - a **diff** from the version before it, in the event itself (an exact line edit
    script; a blob when over 16 KB). Most changes are this: on real edit history it is
    about 6x smaller than whole copies;
  - a **whole copy** in `blobs/`: a file's first version, every 32nd version (so no
    chain of diffs is longer), a rewrite whose diff would be over half the file, and
    binary content;
  - **git's own object**: the first look at the tree (the baseline every later change
    diffs against) copies nothing for tracked files git holds unchanged; the first
    event that changes one names the object (`base_git`). If history is rewritten and
    git collects that object, only that oldest state is lost — its hash stays.

  Any version can be rebuilt by walking its diffs back to a whole copy or git object,
  and is checked against its hash.

* Change notes. Every file an agent reads is noted; when anyone else changes it
  (another agent, a shell command, an editor), the reader's next model call starts
  with one `[changes]` line per file — `src/api.rs — agent-3 (edit, +4 -1)` — so it
  re-reads before building on a stale view. Declared areas: while a writer runs
  (a subagent with `writes`, §5.7, other than `.`), its areas are claimed, and an
  `edit`/`write` by another agent inside a claimed area gets a note that it may
  collide (advice, not a lock).

There is no undo command; git is the undo. The edit tool's "undo loop?" note uses these
records: it fires when an edit returns a file to a state it was in earlier in the same
agent's task.
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
| `log_search` | `(pattern, context_lines?)` | harness (the ledger's tool outputs) |
| `ask_user` | `(question, options?)` | is itself a prompt |

The orchestration tools sit outside this roster: the root agent's `agent` and `tasks` (§5.7),
the subagents' `split` and `fork` (§5.8), and `decide` and `monitor`.

**Minimal output contracts.** The transcript is append-only between compactions (§8),
so a tool result is not paid for once — it is paid for on every request that follows it
in the task. Every tool therefore returns a distillate designed to sit in context
without regret; the raw stream never enters the transcript. Shared rules:

1. **Success is one line** — `exit 0 in 2.3s (14 warnings)`,
   `applied at line 45 — diagnostics clean`. Never echo what the model already knows:
   its own edit text, the command it ran, the content it wrote.
2. **Failure returns the extraction, not the stream** — the error-aware regions (§6.3),
   sized by `tail_lines`.
3. **Everything is recoverable** — full output goes to the ledger keyed by an id;
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
* **Argument shapes** — models send flat or nested variants of these schemas (`{"file": …}` for a
  read, a bare `{"command": …}`, an edits list inside an edits list). `tools/normalize.rs`
  rewrites unambiguous variants to the canonical shape as a call arrives, before it is stored,
  so a shape slip costs nothing and the history shows the right form. A failed `edit` names the
  closest window of the file (numbered, as it is now) or the line numbers of duplicate matches,
  so the retry needs no re-read; text that looks like a compaction placeholder is refused.
  `rizin` with no session reopens the last binary analyzed in the project.
* **`execute_command`** — takes a **steps array**; each step is a real shell script.
  Chains (`&&`, `||`, `;`), `$(…)`, loops, heredocs and multi-line bodies are legal: the
  ban on them existed for per-segment permission checks, which the namespace sandbox
  replaced (PERMISSIONS.md). `shell.rs` still refuses what would hang or misbehave in
  the step runner — a bare `&` (use `background: true`), `&>` under plain sh, an
  unterminated heredoc or quote — and runs the shell's parse-only mode (`-n`) first so
  a syntax error returns at once instead of as a timeout. Separate steps remain the way
  to get each part's own exit code and output. Steps run sequentially in
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
* **`log_search`** — grep over every tool output in the ledger, newest first;
  `log#N` fetches that output. The recovery path that makes aggressive `tail_lines`
  defaults safe.

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
values — never the raw debugger transcript, which goes to the ledger as usual.

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
PERMISSIONS.md §1.2 lists what the model must always ask. `/afk` toggles unattended
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
once per session; `/sysinfo` displays it in the TUI.

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
then fail over. `/model <name>` pins a model for the session.

### 5.5 Plan mode: skeleton plans

For large tasks, entered with `/plan` (the agent may also propose it). The plan
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

* Plan mode is entered with `/plan`. The typecheck gate always applies; skeleton
  approval is `:approve` when attended and auto-granted after the gate in AFK.
* Status bar shows **PLAN**. Review is holistic: browse the skeleton, edit it directly,
  or steer in chat.
* `:approve` exits plan mode into **fill-in**; rejecting is steering (say what to
  change) or discarding the skeleton yourself (git).
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

Every transition is a `session` event in the ledger (§1, §9); crash-resume
replays the journal to the last stable state and continues on the persisted transcript.

### 5.7 Subagent readiness (deferred feature, binding constraints)

Subagents/swarms are **not** in v1, but v1 is built so they bolt on. When they arrive,
a subagent is **a tool call** — `agent(brief, writes)` → a minimal report — and the
parent's transcript sees only the distillate: context isolation is the minimal output
contract applied to a bigger tool. To keep that true, v1 must obey:

1. **`AgentLoop` is a composable unit** — `(brief, toolset, model, budget)` in,
   report out; no process-global state. Spawning is calling the constructor twice.
2. **Ids everywhere** — every UiEvent, journal line, ledger entry, and `log#` key
   carries an agent id (always the root agent in v1). Fields now, not a migration later.
3. **Permissions follow declared write areas** — a subagent whose brief declares no
   `writes` is a reader: it has every tool but `edit` and `write`, and its commands,
   background ones included, run with the project mounted read-only (build-output
   directories excepted), so it can build, test, debug and inspect but not change the
   project. One that declares areas (files or directories, `.` for the whole project)
   is a writer with every tool; its areas are registered while it runs (§3.3), so
   other agents are warned off them. What kind of work a task is (§5.8 labels) never
   changes what it may do.
4. **Children never prompt** — no `ask_user`, no network prompts from subagents;
   anything unresolvable goes in the report and the parent or user decides. Children
   are structurally AFK + read-only, so no approval plumbing crosses agent boundaries.
5. **Spawn brief** — a compact task/state summary handed to the child; the same shape
   compaction (§8.3) will eventually produce.

Config: `[subagent] model, max_turns` (default the parent's model, 30 turns); with
DEALS on (§5.8) the model is only where tasks enter the pool. Deliberately undesigned:
inter-agent messaging beyond change notes (§3.3).

**The pipeline.** With DEALS on (§5.8, `[deals] pipeline`, default on), no model reads the user's message first: it goes straight into the pool as a task, labelled like any other, that may write anywhere in the project, and its report is the reply. The session's root agent never calls a model; it only hands messages to the pool and records the exchange. The next message follows up with the same pool agent, its context intact (a fresh one, after a resume, gets the last few exchanges instead). Parallelism comes from inside the pipeline: the agent working on the message can `fork` independent parts, which run at the same time on their own stations. The AFK verify gate (§5.3), a `/goal`, and anything typed while the task ran all continue the same agent with what they found. The agent answering the user gets its own addendum (PROMPT.md): its final message is the reply. An earlier design put a lead model in front that filed tasks: it cost a model call on every step and its context on every call, it bypassed delegation whenever it could (writing results with Python under `--no-sandbox`), and stripped of work tools it read subagents' logs instead (180 `log_search` calls on one Harness-Bench task) or said it would delegate without doing so. With `pipeline = false`, or no pool, the root agent works on the message itself with every tool and may file pool tasks with `agent` and watch them with `tasks`.

`agent` returns at once: the child runs in the background as a job listed with the monitors (so headless runs and the goal gate wait for it), several children run concurrently, and the report arrives as a `[agent-N <kind> … finished]` wake (with DEALS on, §5.8, a task may first wait in a station's queue) — injected mid-task if the filing agent is still working, or starting a new turn if it ended its turn to wait. A report is headed `[reader agent-N, …]` or `[writer agent-N, …]`, or with DEALS by the task's labels (`[analyze·finance agent-N on …]`); calling `agent` with `agent: "agent-N"` replays that child's conversation from the ledger (agent N's events in this session) and runs the new brief as its next task, so an agent can direct one subagent through several steps without re-explaining. A follow-up keeps the child's write areas unless it gives `writes`, so a reader that found a bug can be told to fix it.

### 5.8 DEALS: subagent tasks served by a model pool

With `[deals] enabled = true`, subagent tasks no longer start on the subagent
model. They queue at **stations** and are routed by DEALS (Decentralized
Expertise-Aware Load Serving, arXiv 2609.33768), extended with a cost term.
Code: `src/deals/`.

* **Stations** are models. `tursi --stations probe` reads Cloudflare's model
  catalog, keeps the tool-calling text models with at least a 128k context and a
  listed price, and gives each a two-turn tool-use check (call a tool, then use
  its result). The survivors, with their prices, paid-tier flag and capability
  tags (`vision`, `reasoning`, `async_queue`, …), are saved to
  `~/.tursi/deals/stations.json`. `[deals] stations = [...]` pins the pool and
  `exclude` drops models from it. Station models with no `[models]` entry get
  `reasoning_effort = "low"` (on reasoning models) and `max_tokens = 8192`, and
  their catalog prices are added to the price table.
* **Tasks** are a brief, the areas it writes (`writes`, which set its permissions,
  §5.7) and optional `needs` (`vision`, `reasoning`, `long_context` = at least 500k
  tokens). Only stations that have every need are eligible: the paper's
  eligible-neighbor set. Nobody says what kind of task it is.
* **Labels** (src/deals/labels.rs). Where the paper has one task type from a small
  fixed set, every tursi task is labelled at intake by the decision model, in one
  call: the user's messages (the pipeline) and tasks filed with `agent` as they come in, fork subtasks while
  the fork is checked. The labels are
  * an **activity**, the kind of work: new, change, debug, test, refactor,
    optimize, review, research, analyze, process, plan, writing, build;
  * a **domain**, what it works on: systems, backend, frontend, data, analytics,
    scripting, infra, ops, binary, security, browser, multimodal, office, finance,
    legal, prose;
  * a **difficulty** from 0 to 4 (trivial, easy, moderate, hard, very hard), the
    probability-weighted level rather than the top one.

  Each option carries a one-line description, and the domain question asks for the
  thing being made or changed, not the topic it serves (code that handles payments
  is backend). The lists cover coding and the office, data, ops and research work in
  Harness-Bench. On its 106 tasks Clef gave an acceptable activity 95% of the time
  and an acceptable domain 97%, and on 58 coding briefs 91% and 98%, with identical
  labels on repeats. Its difficulty agreed with hand labels on the coding briefs at
  Spearman 0.98 but barely separated Harness-Bench's own easy/medium/hard tags (0.22).
  A label the call can't give is left out, and routing leans on the rest.
* **Queues and slots.** Each station keeps a FIFO queue per activity and has C
  execution slots: `[deals] slots` (default 9, the paper's value), bounded by
  the model's rate budget (requests per minute / 8, §7.2) and halved while the
  provider is throttling it. `max_running` (24) caps all stations together.
  When a slot is free the station dequeues the head of its longest queue and
  routes it.
* **Success model** (extends the paper's per-type rate). The chance that station s
  succeeds at a task of activity a, domain m and difficulty d is
  `σ(θ[s] + α[s,a] + β[s,m] − κ[s]·(d − 2))`, an additive item-response model. θ[s]
  is the station's ability overall (prior N(0, 1.5²)); α and β are how much better or
  worse it does at that activity and in that domain (priors N(0, 0.75²)); κ[s] is
  the logits it loses per difficulty level (prior N(1.2, 1²)). All are fitted
  together (MAP, coordinate ascent) from the station's outcomes. With one shared
  slope, a small model that is reliable on easy work and collapses on hard work
  looked middling everywhere, so it was never trusted with the easy work it
  does well; its own slope lets it be cheap and right at the easy end. So an untried
  station is even odds at moderate; a station strong everywhere starts strong at work
  it hasn't tried; an outcome at analyze·finance teaches something about analyze and
  about finance separately, which is what lets a few hundred outcomes cover 13 × 16
  combinations; and easy wins and hard losses together pin down where a small
  model's limit is. Judged QA probabilities are fractional observations.
  `tursi --stations` shows each station's predicted success at easy/moderate/hard
  and the activities and domains it does clearly better (+) or worse (−) at.
* **Routing** (replaces the paper's Eq. 3). Quality first, then cost: of the
  stations that could take a task (the one holding it and every eligible one but
  the station it just came from, which still counts toward the best), keep those
  whose predicted success for its labels is within `tolerance` (0.03) of the best
  one's, and send it to the
  cheapest of them, by expected dollars at its difficulty. A cheaper station
  never wins by being less likely to get the task done; cost only decides between
  stations about as likely as the best. Between stations that also cost about the
  same (within 10%), the shorter backlog (queued plus running) of the task's
  activity wins, with the holder credited one task so equals don't trade work. So
  easy work goes to the cheapest station that is about as reliable as the best at
  it, and hard work goes to whichever station is most likely to finish, whatever
  it costs. A task moves at most `hops` (3) times; no model call decides where it
  runs. The paper's weighted score, `(B_i − B_j) + V·(q_j − q_i) − W·ln(c_j / c_i)`,
  traded success for price: on Harness-Bench run 5 it kept sending tasks to
  gemma-4-26b (0.71 at moderate against glm-5.3-flash's 0.82) because it was
  cheaper, and those 12 tasks scored 0.10 below the glm-5.3-flash baseline.
* **Exploration** (not in the paper, which explores only in its warm-up split).
  For training runs only (`explore`, off by default; `--explore-min` turns it on),
  routing compares a draw from each station's
  learned success instead of the estimate itself: Thompson sampling over the
  success model's posterior, approximated as normal around the fit with the
  covariance from the curvature there (the joint one, so a combination a station
  has done often is as certain as its record says). A station with few outcomes
  draws widely and sometimes wins, so it gets tried; one that has shown it fails
  draws low and is left alone. A task draws once per station and keeps those draws
  across its hops, so it doesn't bounce between stations. Without exploration
  DEALS only ever picks the current best: on the first Harness-Bench slice (35
  tasks, estimates reset) 79 of 81 subagent tasks went to glm-5.3-flash and 13 of 15
  stations never ran one.
  Real work routes on the estimates: a draw can put a dearer station that is no
  better above the band and take the task from a cheaper equal, which a training
  run pays to learn from and real work shouldn't.
* **Cost model.** A station's expected dollars for a task is its price-based guess
  (typical tokens at its prices) times a curve in difficulty fitted over every station's
  observed costs, times the station's own factor, shrunk toward 1 until it has
  outcomes. Real cost ran from about 0.1× the guess on trivial tasks to 0.7× on very hard
  ones (doubling per level), and reasoning-heavy models ran several times the curve
  (deepseek-v4-pro 3×). An earlier single scale, learned mostly from a cheap model's
  easy tasks, priced deepseek-v4-pro's first hard task at $0.04; it cost $0.37.
* **Probation** (not in the paper): until a station has `probation` (3) outcomes,
  routing sends it a task only while it holds none, so an untried model learns on
  one task instead of a whole burst.
* **Ingress** is the `[subagent]` model when that is an eligible station,
  otherwise the least-loaded eligible one. Follow-ups (`agent: "agent-N"`) are
  labelled afresh and go back to the station that last ran the task, with its
  transcript, and are not routed.
* **Splits.** Subagents have a `split` tool: they hand back what they finished
  and what remains. The continuation re-enters the same station's queue carrying
  the finished parts, and the next station resumes from those results as a fresh
  subagent. After `splits` (3) splits the task ends unfinished.
* **Time limits.** Each segment has a wall-clock limit by difficulty:
  `segment_secs` (300) for a moderate task, doubling per level above and halving
  per level below, within 1 and 30 minutes (trivial 75 s, hard 600 s, very hard
  1200 s). Past it the subagent stops at its next turn. Running out of turns or
  time counts as a split, with two differences from a voluntary one. The station
  is learned from at once, with a weak failure (0.2), and leaves the task's final
  credit, so a run that is cut off later (the process killed) still teaches. And
  the continuation must move to another eligible station, whatever the scores and
  hops say. Before this, exploration sent Harness-Bench data tasks to
  glm-4.7-flash (about 30 s a turn), which ran 520 s and 940 s on single segments,
  and the bench killed both tasks at 1200 s without the pool learning anything.
* **Memory** (paper §3.3, Eqs. 4–5). Each station keeps up to `memory_cap` (40)
  successful trajectories per project in `.tursi/deals/memory/<model>.jsonl`:
  the brief, one line per tool step, the report, the time taken, and a bge-large
  embedding of the brief (`embed_model`, Workers AI). Before a segment runs, up
  to `memory_k` (3) past successes with relevance
  `cos · (1 + 0.2 / (1 + minutes)) ≥ memory_theta` (0.7) are prepended to the
  brief as demonstrations.
* **QA and learning.** When a task leaves the pool, the decision model
  (`[decide]`, §7) reads the brief, its activity and whether it was read-only, how
  it ended, the commands run with their
  ✓/✗ marks, the files changed, and the report. It answers one yes/no question:
  was the brief accomplished? The probability p is the verified outcome. It is
  capped at 0.4 for a turn-limit ending, and without a decision model the ending
  alone sets it (done 0.7, turn limit 0.2, failed 0). Every station that executed
  part of the task adds p successes and 1 − p failures. The outcome is appended
  to `~/.tursi/deals/outcomes.jsonl`, and the estimates are a fold over that log,
  so concurrent sessions never clobber each other. Unlike the paper, estimates
  never freeze. Runs with p ≥ 0.7 join the memory. A report filed with `agent` is
  headed with the labels, the station and the QA figure: `[new·backend agent-3 on
  glm-5.3-flash, 95s, QA 0.82 — …]`. The judge also reads what the changed files now
  hold (up to six, each whole up to 2,500 characters or else its first 1,500), and is
  asked to check them against the brief's requirements. On Harness-Bench this did not
  make it track the bench's own scores (Spearman about 0.12 with or without the
  contents, and a reasoning chat model judging the same evidence did no better): whether
  a CSV of reconciled totals is right takes recomputing it, which no judge here does.
* **Ground truth.** Where a task has a real grade (a benchmark's oracle), it replaces
  the judge: `bench/feedback.py` appends a `truth` record for the project the task ran in
  (`{project, truth, source}`), and the fold credits it by role. The project's last
  judged outcome, the one that produced the deliverable (with the pipeline usually the
  only one), counts with the true score; earlier ones are capped at it but never raised,
  so a failed attempt stays a failure when a later one did the job, and the judge's
  generosity is pulled down to the truth. Outcomes recorded when a segment ran out
  (`partial`) are left alone. The judged values stay in the log, for calibrating the
  judge later. `bench/harness-bench.sh` runs the feedback after every task.
  `tursi --pool <ids>` limits the pool for one run: `bench/bench-tursi-glm.sh` is the
  single-model baseline the router must beat on $/solved.
* **Fork** (an extension: the paper only splits sequentially, and its parallelism
  comes from separate tasks arriving together). A subagent whose brief, or what
  remains of it, is several pieces that need nothing from each other can `fork`
  them: up to `fork_width` (8) subtasks, each a standalone brief, plus `then` —
  what the continuation does with their results. A subtask that changes files
  declares them in `writes`: inside the forking agent's own areas (a reader can fork
  only readers), never the whole project, and no two overlapping; one that declares
  none is a reader. Before the fork goes through, the decision model is asked whether
  every subtask can be done without another's result (below 0.35 the fork is refused
  with that reason) while each subtask is labelled in its own call. The subtasks
  enter the forking station's queues as ordinary tasks: routed, run, judged and
  learned from each under its own labels, so credit lands on the station that did
  each piece. The parent waits outside the queues; when the last subtask finishes,
  its continuation is queued with every subtask's report, QA figure and changed
  files, and runs like any continuation. A fork counts as one of the task's
  `splits`. By default subtasks cannot fork again (`fork_depth` = 1); a fork that
  isn't allowed becomes an ordinary split with the subtasks listed as work to do in
  order. `[deals] fork = false` turns it off. Every writer's areas are registered
  while it runs (§3.3), and agents working in a shared checkout are told when files
  they read change under them.
* **Back pressure.** Past `max_queued` (64) waiting tasks, `agent` refuses new
  ones and says to wait for reports. Otherwise it answers
  `started on <model>` or `queued at <model> (N ahead)`.
* **Warm-up** (the paper's training split). `bench/deals-warmup.py` runs the
  graded fixtures in `bench/warmup/` (new code, a bug, tests with mutation checks,
  docs, two lookups, a review), each labelled once by the decision model and the
  labels kept in its `meta.json`, on each station with `tursi --station <model>`,
  which runs that model alone with full tools and DEALS off. It appends the graded
  outcomes to the same log.

`tursi --stations` lists the pool with prices, tags and what each station has
learned.

## 6. Execution, Sandboxing & Adaptive Limits

### 6.1 Tool schema

```json
{
  "name": "execute_command",
  "description": "Run commands in the sandbox. Steps run sequentially in ONE shell: cd and exports persist across steps (not across calls). Each step is a shell script: chains, pipes, redirects, $(…) and heredocs are fine; a trailing & is refused (use background).",
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
* Full output always goes to the ledger (redacted).
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
A `[providers.<prefix>]` entry there may set `base_url` and a `headers` table, which is
how a gateway (e.g. Cloudflare AI Gateway's `cf-aig-gateway-id`) is used; the model
string keeps everything after the prefix. `[decide] model = "…"` names a decision model (Cloudflare Clef/Clef-flash, Jev): a
discriminative model that answers typed questions (yes/no, choice, score) over a state with
calibrated probabilities. It backs the `decide` tool and, with `shadow = true` (default),
logs what a command-risk gate would have decided to `~/.tursi/decisions.jsonl` without
acting (DEALS uses it to label tasks and judge outcomes, never to pick a model, §5.8); `bench/decisions.json` is the labelled set
(`cargo test live_labelled -- --ignored`). A project's `.tursi/config.toml` may set `model`
(and `fallbacks`, `[models]`) to run that project on another model, and `--model <id>`
overrides both for one run. `[models."<id>"] reasoning_effort = "…"` in
config.toml is sent with every request to that model; thinking is billed as output,
so this is the main cost knob for reasoning models.
Responses stream (SSE). When a streamed turn ends with reasoning but no text and no tool call, the same request is retried once without streaming: some providers' stream parsers lose a reasoning model's tool call into the reasoning channel (Cloudflare's gpt-oss-20b), and the whole reply comes back intact. There is no router for the root model: no tiers, no escalation, no summarized handoff (§5.4).
`:model <name>` pins a model for the session. Subagent tasks are routed across a model pool
when DEALS is on (§5.8).

### 7.2 Rate limits

Providers limit requests per account, so tursi keeps one token bucket per model and one per
provider gateway, shared by every agent in the process (`src/ratelimit.rs`). Every model
call waits for both. `[limits]`: `paid_rpm` (50, Cloudflare's per-model cap for paid-tier
models on unified billing) applies to models the station catalog marks paid, `default_rpm`
(300) to the rest, `rpm."<id>"` overrides one model, and `gateway_rpm` (200, AI Gateway's
unified-billing cap) covers all models together. Buckets allow a ten-second burst. A 429
empties the model's bucket and stalls it for 20 s. The call retries up to six times instead of
failing over, and DEALS halves that station's slots for 90 s.

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
   the oldest turns and collapse mid-age tool outputs (recoverable from the ledger).
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

* **Session record:** every session's state transitions, conversations, outputs and
  file changes are events in the project ledger (§1). After a crash, `--resume`
  replays the conversation from it.
* **Stats ledger:** each API call appends `{ts, session, agent, task, model, in_tokens,
  cached_tokens, out_tokens, cost}` to the monthly JSONL in `~/.tursi/stats/` (`agent`
  is the root agent until subagents exist — §5.7). The status-bar session
  and monthly figures are computed from these ledgers using the config price table.
* **Budgets:** optional soft caps in config (`session_usd`, `month_usd`) — the harness
  warns at 80% and requires explicit confirmation to continue past 100%.

What the user sees is the provider's prepaid balance, not the harness's token arithmetic: `balance.rs` reads it (derived for Cloudflare AI Gateway from the provider base URL; `[balance] url/pointer/divisor` for others) at session start, after every task, and at most every 20 s mid-task, and the status bar (`Bal $8.42`), the ✻ footer (`bal $8.42`), `--check` and the headless JSON (`balance_usd`) show it. The ledger and budgets still use the price table underneath; where no balance endpoint exists the old cost figures are shown.

## 10. Out of Scope (v1)

* Windows/macOS sandboxing (Linux-only; macOS would need Seatbelt, Windows has no
  namespace equivalent).
* Parallel multi-task execution. The in-place edit model assumes one active task per
  project.
* Worktrees for parallel writers. Workers share the checkout today and rely on
  staleness checks (§3.2). Two writers editing the same file conflict: one edit
  fails and that writer re-reads.

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
