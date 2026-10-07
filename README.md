# tursi

A local autonomous coding agent with a terminal UI. It edits your project
directly, runs everything it executes inside a per-session sandbox, and is
built to spend as few tokens as possible.

## Install

Needs Rust and a Linux kernel with unprivileged user namespaces.

    cargo install --path .

The first run writes `~/.tursi/config.toml` with a starter model. Put your
API key in the environment (`DEEPSEEK_API_KEY`) or in
`~/.tursi/secrets.toml` (chmod 600). Check the setup with:

    tursi --check

## Use

Run it inside a project directory:

    tursi

Type a task and press `Enter`. `Esc` interrupts a running task, `/help` lists
the commands, `Ctrl+C` quits. The chat shows your messages and the replies.
Beside it is a block of braille, and every agent at work lights one dot in it:
blue for one that only reads, green for one that changes files.

Other ways to run it:

    tursi --task "fix the failing test" --json   # one task, no UI, metrics as JSON
    tursi --sessions                              # list this project's sessions
    tursi --resume                                # continue the most recent session
    tursi --resume 01a1038b                       # continue a session by id prefix

## What it can do

The model has tools for reading, editing and searching files, running
commands, a language server for definitions and diagnostics, gdb, rizin, and
a profiler. It can run long commands in the background and keep working, and
it can arm a monitor on files or a command and be woken when something
happens instead of polling. When it is torn between options it can ask a
small decision model for calibrated probabilities instead of guessing.

## Model pool

With `[deals] enabled = true` in `~/.tursi/config.toml`, subagent tasks go to
a pool of models instead of one fixed model. A decision model labels each task
with the kind of work (new code, debugging, analysis, writing and so on), what
it works on (backend, data, finance, office and so on) and how hard it is. The
task then runs on the cheapest model that is about as likely as the best one
to get work like it done right. The decision
model grades every finished task, and the pool learns from those grades.

    tursi --stations probe          # find and test the models that can join
    tursi --stations                # what each model has learned so far
    bench/deals-warmup.py --cheap   # teach the pool with small graded tasks

SPEC.md section 5.8 has the details.

## Safety

Everything the agent runs is contained by a sandbox. It can write only to the
project, a private /tmp and the package caches. The rest of your home does
not exist inside. Network reaches only package registries unless you approve
more for a single call. An agent working on your message may change any file in
the project. A subagent another agent hands work to changes files only when it
is given the files or directories it may write; without that list it sees the
project read-only. Details are in PERMISSIONS.md.

Every file change is recorded in the project's ledger, `.tursi/ledger.jsonl`, with
the content before and after and which agent made it, including changes made by
shell commands.

## Configuration

Global: `~/.tursi/config.toml` (model, prices, budgets, network and sandbox
additions). Per project: `.tursi/config.toml` (verify and typecheck commands,
language server overrides).

## More

SPEC.md is the design. PERMISSIONS.md covers the sandbox and the network.
PROMPT.md is the system prompt. BENCH.md is the benchmark protocol.
