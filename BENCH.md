# tursi — benchmark protocol

How tursi's efficiency claims get falsified or confirmed against other harnesses.
Lives alongside SPEC.md; implemented as `bench/` (runner scripts + task containers),
independent of the harness itself.

## Comparators

- tursi (default config; `--task --no-sandbox` inside the task container)
- Claude Code (headless: `claude -p`)
- aider
- OpenCode / Crush (or current equivalents)
- **naive-loop control**: a ~100-line raw API loop with read/write/bash tools — the
  floor that tells us what any harness adds over nothing.

All harnesses run full-auto (no human in the loop); a run that stalls on interactive
input past the task timeout is a fail.

## Measurement: at the API boundary

Every harness runs through a local metering proxy that records, per request: model,
input/cached/output tokens, latency, and computed cost from one shared price table.
Nothing is taken from harness self-reporting — tursi's own ledger (§9) serves only as
a cross-check of the proxy. The proxy log is the single source of truth for every
cost metric.

## Modes

| Mode | Setup | Isolates |
|---|---|---|
| **A — harness efficiency** | Same single model in every harness | Context bloat, round-trips, edit-retry waste — the harness itself |
| **B — full system** | Each harness in native best config | Real-world pass-rate-per-dollar |

## Suites

1. **SWE-bench Verified (subset)** — comparability with published numbers. Treated as
   relative-only: these tasks are in every model's training data.
2. **Terminal-Bench (subset)** — terminal-native agent tasks.
3. **Systems suite (custom, 10–20 tasks)** — the differentiator, authored fresh so it
   is post-cutoff for every model. Rust/C tasks with machine-checkable outcomes.
   Archetypes:
   - fix a data race that a stress test catches intermittently
   - find and fix a use-after-free / buffer overflow from a failing ASan run or core
     dump (`debug`, rr territory)
   - make a hot function ≥N% faster, verified by hyperfine baseline-vs-patch
     (`profile` territory)
   - fix a build broken against a pinned toolchain
   - implement a small feature in an existing codebase with held-out tests
   - reduce peak RSS of a workload below a stated bound

## Run protocol

- One fresh container (pinned toolchain image) per task per run; repo snapshot
  mounted; network limited to the model proxy plus package registries.
- n = 5 runs per task per harness; report pass@1 as mean ± 95% CI. Temperature pinned
  where the harness exposes it.
- Per-task wall-clock timeout (suite-dependent, default 30 min); crash or timeout = fail.
- **Grading is external**: held-out tests are restored from outside the container
  after the harness exits, then run. Editing the tests cannot produce a pass.
- Competitor harnesses run stock defaults — their defaults are their product.

## Metrics (per harness × suite × mode)

| Metric | Definition |
|---|---|
| pass@1 | solved on first run, mean over n runs |
| **$/solved** | total spend ÷ solved count — the headline; $/task flatters cheap failure |
| tokens/task | input, cached, output, reported separately |
| cache hit rate | cached ÷ (input + cached) |
| round trips | API requests per task |
| wall time | start → harness exit |
| edit failure rate | failed/retried edit operations ÷ total edits (from harness logs where available) |

## Output

`bench/results/*.jsonl` — one line per (task, harness, mode, run) with all metrics;
a report generator renders the comparison table and the cost-vs-pass-rate frontier
plot. The systems-suite table graduates to the README once numbers exist.

## Honesty rules

- Publish losses with wins; the frontier plot shows every harness, not a curated cut.
- Any tursi-favoring deviation from stock competitor config is disclosed in the table
  footnotes, or the run is invalid.
- Contaminated-suite results (SWE-bench) are never quoted as absolute capability
  claims, only as same-suite relative comparisons.
