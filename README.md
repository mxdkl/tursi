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

Press `i` to type a task, `Enter` to send it, `Esc` to interrupt a running
task, `:help` for the keymap, `:q` to quit. `Ctrl+O` expands tool output.

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
happens instead of polling.

## Safety

Everything the agent runs is contained by a sandbox. It can write only to the
project, a private /tmp and the package caches. The rest of your home does
not exist inside. Network reaches only package registries unless you approve
more for a single call. Details are in PERMISSIONS.md.

Edits are checkpointed. `:rewind` restores the last change, and checkpoints
survive a resume.

## Configuration

Global: `~/.tursi/config.toml` (model, prices, budgets, network and sandbox
additions). Per project: `.tursi/config.toml` (verify and typecheck commands,
language server overrides).

## More

SPEC.md is the design. PERMISSIONS.md covers the sandbox and the network.
PROMPT.md is the system prompt. BENCH.md is the benchmark protocol.
