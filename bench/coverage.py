#!/usr/bin/env python3
"""Build a benchmark set that covers every activity x domain x difficulty
combination the available tasks reach, and the plan that gives every model a
few tries at each one (SPEC 5.8, training runs).

  bench/coverage.py inventory <harbor_datasets_dir> <harness_bench_dir> -o candidates.jsonl
  bench/coverage.py label candidates.jsonl -o labelled.jsonl [-j 8]
  bench/coverage.py select labelled.jsonl [--per-cell 3] -o set.jsonl
  bench/coverage.py plan set.jsonl -o plan.jsonl [--models a,b,...]
  bench/coverage.py run plan.jsonl --results r.jsonl --jobs <dir> --budget <usd> [-j 4]

inventory: every task of the datasets in DATASETS (Harbor exports, as
`harbor download <org>/<name> -o <dir>/<org>-<name>` writes them) and the
Harness-Bench tasks, with the instruction the agent will see.

label: `tursi --label` on each instruction, so the cells are the ones the
router will put the task in. Resumable: labelled ids are skipped.

select: up to --per-cell tasks per (activity, domain, difficulty level),
spread over sources, and a report of the cells nothing reaches.

plan: one trial per (task, model), every station in the pool, with the cost
expected from each model's past outcomes at that difficulty.

run: each trial as `--pool <model>` (Harbor through bench/harbor_tursi.py,
Harness-Bench through bench/harness-bench.sh), so the planned model does the
work and the router learns from the benchmark's grade. Resumable; stops once
the real balance has dropped by --budget.
"""
from __future__ import annotations

import argparse
import collections
import concurrent.futures as cf
import json
import random
import subprocess
import sys
import tomllib
from pathlib import Path

REPO = Path(__file__).resolve().parent.parent
TURSI = REPO / "target/release/tursi"
LEVELS = ["trivial", "easy", "moderate", "hard", "very hard"]


def _text(task: Path) -> str:
    return (task / "instruction.md").read_text(errors="replace") if (task / "instruction.md").exists() else ""


# Harbor datasets that run with tursi in the container: test-graded, or
# LLM-judged with a judge we point at the Cloudflare gateway (judge=True).
# cap: most tasks taken (seeded sample) before labelling; keep: which tasks
# are usable, from the survey (scratchpad harbor-survey/*.md).
DATASETS = {
    # office and business
    "blobfishai-hubbench": {"cap": 60},
    "spreadsheetbench-verified-1.0": {"cap": 120},
    "agentic-labs-erp-bench": {"cap": 60},
    "benchflow-skillsbench": {},
    "blobfishai-erpbench-100-suite": {"cap": 40},
    "blobfishai-dealbench-100-suite": {"cap": 40},
    "blobfishai-salesbench-100": {"cap": 40},
    "blobfishai-factorybench-100": {"cap": 30},
    "openthoughts-tasktrove-nemotron-gym-agent-calendar": {"cap": 60},
    # browser
    "blobfishai-webbench": {"cap": 80},
    # finance
    "adyen-dabstep": {"cap": 120, "keep": lambda t: not any(x in t.name for x in ("2521", "2522", "2566"))},
    "blobfishai-ledgerbench-100": {"cap": 40},
    "dissei-financial-judgment-full": {"judge": True},
    # legal
    "punitarani-harvey-labs": {"cap": 150, "judge": True},
    "blobfishai-counselbench-100": {"cap": 30},
    # multimodal
    "mmtb-multimedia-terminalbench": {},
    "futurehouse-labbench": {"cap": 60},
    "lica-world-gdb": {"cap": 80},
    "thetalab-vector-edit-gym": {"keep": lambda t: any(w in _text(t).lower() for w in ("remove", "delete", "flip", "mirror", "duplicate"))},
    "gaia-gaia": {"keep": lambda t: any(p.name != "Dockerfile" for p in (t / "environment").rglob("*") if p.is_file())},
    # prose and research
    "ivanleo-agent-search": {"judge": True},
    # ops and infra
    "grafana-o11y-bench": {"judge": True},
    "MichaelY310-devopsgym": {"keep": lambda t: t.name.startswith("monitor")},
    "blobfishai-devopsbench-100": {"cap": 40},
    "quesma-compilebench": {},
    # security and binary
    "polyvorlabs-cyberdefense-bench": {"cap": 60},
    "polyvorlabs-revbench": {},
    # data and analytics
    "dbt-labs-ade-bench": {},
    "snowflake-labs-data-eng-bench": {"cap": 60},
    "scale-ai-hil-bench": {"cap": 60, "keep": lambda t: "ask" not in t.name and "full_info" not in t.name},
    # systems, backend and general coding
    "crustbench-crustbench": {"cap": 60},
    "quesma-otel-bench": {},
    "abundant-swe-gen-go": {"cap": 60},
    "abundant-swe-gen-rust": {"cap": 60},
    "aider-aider-polyglot": {"cap": 80},
    "openthoughts-openthoughts-tblite": {},
    "terminal-bench-terminal-bench-2-1": {"keep": lambda t: not any(w in t.name for w in ("qemu", "windows", "chromium"))},
}


def inventory(args) -> None:
    rows = []
    rng = random.Random(7)
    root = Path(args.datasets)
    for name, spec in DATASETS.items():
        tasks = sorted(p.parent for p in (root / name).glob("*/*/task.toml"))
        tasks = [t for t in tasks if _text(t) and spec.get("keep", lambda t: True)(t)]
        if "cap" in spec and len(tasks) > spec["cap"]:
            tasks = sorted(rng.sample(tasks, spec["cap"]))
        for t in tasks:
            toml = tomllib.loads((t / "task.toml").read_text())
            rows.append({
                "id": f"harbor:{name}/{t.name}",
                "source": name,
                "path": str(t),
                "judge": bool(spec.get("judge")),
                "timeout": (toml.get("agent") or {}).get("timeout_sec"),
                "own_difficulty": (toml.get("metadata") or {}).get("difficulty"),
                "brief": _text(t),
            })
    hb = Path(args.harness_bench)
    import yaml  # Harness-Bench's own task format
    for d in sorted(p for p in (hb / "tasks").iterdir() if (p / "task.yaml").exists()):
        t = yaml.safe_load((d / "task.yaml").read_text())
        files = [t.get("prompt_file")] if t.get("prompt_file") else (t.get("prompt_files") or sorted(p.name for p in d.glob("prompt*.txt"))[:1])
        if not files or not (d / files[0]).exists():
            continue
        rows.append({
            "id": f"hb:{t['task_id']}",
            "source": "harness-bench",
            "path": str(d),
            "judge": False,
            "timeout": t.get("timeout_sec"),
            "own_difficulty": t.get("difficulty"),
            "brief": (d / files[0]).read_text(errors="replace"),
        })
    with open(args.o, "w") as f:
        for r in rows:
            f.write(json.dumps(r) + "\n")
    by = collections.Counter(r["source"] for r in rows)
    print(f"{len(rows)} candidates from {len(by)} sources")
    for s, n in by.most_common():
        print(f"  {n:4} {s}")


def _label(brief: str) -> dict:
    for _ in range(3):
        out = subprocess.run([str(TURSI), "--label"], input=brief, capture_output=True, text=True, timeout=120)
        try:
            labels = json.loads(out.stdout.strip().splitlines()[-1])
        except (json.JSONDecodeError, IndexError):
            labels = {}
        if labels.get("activity") and labels.get("domain") and labels.get("difficulty") is not None:
            return labels
    return {}


def label(args) -> None:
    rows = [json.loads(l) for l in open(args.candidates)]
    done = set()
    if Path(args.o).exists():
        done = {json.loads(l)["id"] for l in open(args.o)}
    todo = [r for r in rows if r["id"] not in done]
    print(f"{len(done)} labelled already, {len(todo)} to go", flush=True)
    failed = 0
    with cf.ThreadPoolExecutor(args.j) as ex, open(args.o, "a") as out:
        for r, labels in zip(todo, ex.map(lambda r: _label(r["brief"]), todo)):
            if not labels:
                failed += 1
                continue
            out.write(json.dumps({**{k: v for k, v in r.items() if k != "brief"}, **labels}) + "\n")
            out.flush()
    print(f"done; {failed} could not be labelled (rerun to retry them)")


def level(d: float) -> int:
    return min(4, max(0, int(round(d))))


def select(args) -> None:
    rows = [json.loads(l) for l in open(args.labelled)]
    rows = [r for r in rows if r["difficulty"] < args.max_difficulty and not (args.no_judge and r["judge"])]
    cells = collections.defaultdict(list)
    for r in rows:
        cells[(r["activity"], r["domain"], level(r["difficulty"]))].append(r)
    rng = random.Random(11)
    chosen = []
    for key in sorted(cells):
        pool = cells[key]
        rng.shuffle(pool)
        # Round-robin over sources so one benchmark doesn't fill a cell.
        by = collections.defaultdict(list)
        for r in pool:
            by[r["source"]].append(r)
        picked = []
        while len(picked) < args.per_cell and any(by.values()):
            for s in sorted(by, key=lambda s: -len(by[s])):
                if by[s] and len(picked) < args.per_cell:
                    picked.append(by[s].pop())
        chosen += [{**r, "round": i} for i, r in enumerate(picked)]
    # Every cell's first task, then every cell's second, and so on: a run
    # stopped by its budget has covered the cells evenly.
    chosen.sort(key=lambda r: r["round"])
    with open(args.o, "w") as f:
        for r in chosen:
            f.write(json.dumps(r) + "\n")
    # Report: domain x level coverage, then cells short of --per-cell.
    domains = sorted({r["domain"] for r in rows} | set(DOMAINS))
    print(f"{len(chosen)} tasks in {len(cells)} cells (activity x domain x level) from {len(rows)} labelled")
    print("\ntasks per domain x level in the set (cells with any task / tasks):")
    print(f"  {'':12}" + "".join(f"{l:>12}" for l in LEVELS))
    for dom in domains:
        line = f"  {dom:12}"
        for lv in range(5):
            ks = [k for k in cells if k[1] == dom and k[2] == lv]
            n = sum(min(len(cells[k]), args.per_cell) for k in ks)
            line += f"{(f'{len(ks)}/{n}' if ks else '-'):>12}"
        print(line)
    short = [(k, len(v)) for k, v in sorted(cells.items()) if len(v) < args.per_cell]
    print(f"\n{len(short)} cells have fewer than {args.per_cell} tasks")
    empty = [(d, LEVELS[lv]) for d in domains for lv in range(5) if not any(k[1] == d and k[2] == lv for k in cells)]
    print(f"{len(empty)} domain x level combinations have no task at all:")
    for d, lv in empty:
        print(f"  {d} {lv}")


DOMAINS = ["systems", "backend", "frontend", "data", "analytics", "scripting", "infra", "ops", "binary", "security", "browser", "multimodal", "office", "finance", "legal", "prose"]


def plan(args) -> None:
    tasks = [json.loads(l) for l in open(args.set)]
    models = args.models.split(",") if args.models else _pool()
    # Past cost per outcome by model and difficulty level, for the estimate.
    seen = collections.defaultdict(list)
    for line in open(Path.home() / ".tursi/deals/outcomes.jsonl"):
        r = json.loads(line)
        if "truth" in r:
            continue
        for m, c in (r.get("cost") or {}).items():
            seen[(m, level(r.get("difficulty", 2.0)))].append(c)
    def expect(m: str, lv: int) -> float | None:
        for near in (lv, lv - 1, lv + 1, 2):
            v = seen.get((m, near))
            if v:
                return sum(v) / len(v) * (2 ** (lv - near))
        return None
    total, unknown = 0.0, set()
    per_model = collections.Counter()
    with open(args.o, "w") as f:
        for t in tasks:
            for m in models:
                c = expect(m, level(t["difficulty"]))
                if c is None:
                    unknown.add(m)
                else:
                    per_model[m] += c
                    total += c
                f.write(json.dumps({"task": t["id"], "source": t["source"], "path": t["path"], "judge": t["judge"], "model": m}) + "\n")
    print(f"{len(tasks)} tasks x {len(models)} models = {len(tasks) * len(models)} trials")
    print(f"expected model cost about ${total:.0f} (from each model's past cost at that difficulty)")
    for m, c in per_model.most_common():
        print(f"  ${c:7.2f} {m.rsplit('/', 1)[-1]}")
    if unknown:
        print(f"no cost history for {', '.join(sorted(unknown))}")


HARBOR = Path("/tmp/claude-1000/-home-player1-tursi-harness/ce9cfd37-37c9-4329-8434-6e2b2ffdd17d/scratchpad/impl/harbor-venv/bin/harbor")


def balance() -> float | None:
    out = subprocess.run([str(TURSI), "--check"], cwd=REPO, capture_output=True, text=True, timeout=60).stdout
    for line in out.splitlines():
        if line.startswith("balance: $"):
            return float(line.split("$")[1].split()[0])
    return None


def _trial(t: dict, jobs: Path, judge_env: dict) -> dict:
    """Run one (task, model) trial; its grade and cost."""
    model = t["model"]
    short = model.rsplit("/", 1)[-1]
    env = {**__import__("os").environ, "TURSI_ARGS": f"--pool {model}"}
    if t["source"] == "harness-bench":
        task = t["task"].split(":", 1)[1]
        out = subprocess.run([str(REPO / "bench/harness-bench.sh"), "task", task], env={**env, "HARNESS": "tursi-deals"},
                             capture_output=True, text=True, timeout=4 * 3600)
        log = out.stdout + out.stderr
        score = next((float(l.split(":")[1].strip().rstrip(",")) for l in log.splitlines() if '"combined_score"' in l), None)
        usage = next((json.loads(l) for l in reversed(log.splitlines()) if l.strip().startswith("{") and "cost_usd" in l), {})
        return {"reward": score, "cost": usage.get("cost_usd"), "error": None if score is not None else log[-400:]}
    name = "".join(c if c.isalnum() or c in "-_" else "-" for c in f"{t['task'].split('/', 1)[1]}--{short}")[:120]
    cmd = [str(HARBOR), "run", "-p", t["path"], "--agent-import-path", "harbor_tursi:Tursi", "-e", "docker",
           "-o", str(jobs), "--job-name", name, "-q"]
    for k, v in (judge_env.get(t["source"], {}) if t["judge"] else {}).items():
        cmd += ["--ve", f"{k}={v}"]
    env["PATH"] = f"{REPO / 'bench/bin'}:{env['PATH']}"
    env["PYTHONPATH"] = str(REPO / "bench")
    out = subprocess.run(cmd, env=env, capture_output=True, text=True, timeout=6 * 3600)
    subprocess.run([sys.executable, str(REPO / "bench/harbor_tursi.py"), str(jobs / name)], capture_output=True)
    results = sorted((jobs / name).glob("*/result.json"))
    if not results:
        return {"reward": None, "cost": None, "error": (out.stdout + out.stderr)[-400:]}
    r = json.loads(results[-1].read_text())
    rewards = (r.get("verifier_result") or {}).get("rewards") or {}
    reward = rewards.get("reward", next(iter(rewards.values()), None) if len(rewards) == 1 else None)
    if reward is not None and reward > 1:
        reward = reward / 100  # some graders score out of 100
    ex = r.get("exception_info")
    return {"reward": reward, "cost": (r.get("agent_result") or {}).get("cost_usd"),
            "error": (ex.get("exception_message") or str(ex))[:400] if ex and reward is None else None}


def run(args) -> None:
    trials = [json.loads(l) for l in open(args.plan)]
    results = Path(args.results)
    done = set()
    if results.exists():
        done = {(r["task"], r["model"]) for r in map(json.loads, open(results)) if r.get("reward") is not None}
    todo = [t for t in trials if (t["task"], t["model"]) not in done]
    # Interleave models so parallel trials spread over the rate limits.
    by = collections.defaultdict(list)
    for t in todo:
        by[t["model"]].append(t)
    order = [t for group in __import__("itertools").zip_longest(*by.values()) for t in group if t]
    jobs = Path(args.jobs)
    jobs.mkdir(parents=True, exist_ok=True)
    judge_env = json.loads(Path(args.judge_env).read_text()) if args.judge_env else {}
    start = args.start_balance if args.start_balance is not None else balance()
    print(f"{len(done)} done, {len(todo)} to go; balance ${start:.2f}, budget ${args.budget:.2f}", flush=True)
    import threading
    lock, spent, stop = threading.Lock(), [0.0], threading.Event()
    def one(t: dict) -> None:
        if stop.is_set():
            return
        import time
        t0 = time.time()
        try:
            r = _trial(t, jobs, judge_env)
        except subprocess.TimeoutExpired:
            r = {"reward": None, "cost": None, "error": "timed out"}
        row = {"task": t["task"], "source": t["source"], "model": t["model"], **r, "secs": int(time.time() - t0)}
        with lock:
            with open(results, "a") as f:
                f.write(json.dumps(row) + "\n")
            spent[0] += r.get("cost") or 0.0
            print(f"{row['reward'] if row['reward'] is not None else 'ERR':>6} ${(r.get('cost') or 0):.3f} {row['secs']:5}s {t['model'].rsplit('/', 1)[-1]:28} {t['task']}"
                  + (f"  [{r['error'][:120]}]" if r.get("error") else ""), flush=True)
            if spent[0] > args.budget:
                now = balance()
                if now is not None and start - now > args.budget:
                    print(f"budget reached: ${start - now:.2f} spent by balance; stopping", flush=True)
                    stop.set()
    with cf.ThreadPoolExecutor(args.j) as ex:
        list(ex.map(one, order))
    now = balance()
    print(f"finished; spent ${start - now:.2f} by balance" if now is not None else "finished", flush=True)


def _pool() -> list[str]:
    """Stations that passed the tool-use probe (`tursi --stations probe`)."""
    stations = json.loads((Path.home() / ".tursi/deals/stations.json").read_text())["stations"]
    return [s["model"] for s in stations if (s.get("probe") or {}).get("ok")]


def main() -> int:
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    sub = ap.add_subparsers(dest="cmd", required=True)
    p = sub.add_parser("inventory"); p.add_argument("datasets"); p.add_argument("harness_bench"); p.add_argument("-o", required=True)
    p = sub.add_parser("label"); p.add_argument("candidates"); p.add_argument("-o", required=True); p.add_argument("-j", type=int, default=8)
    p = sub.add_parser("select"); p.add_argument("labelled"); p.add_argument("--per-cell", type=int, default=3); p.add_argument("-o", required=True)
    p.add_argument("--max-difficulty", type=float, default=5.0, help="only tasks labelled below this (1.5: trivial and easy)")
    p.add_argument("--no-judge", action="store_true", help="leave out LLM-judged sources")
    p = sub.add_parser("plan"); p.add_argument("set"); p.add_argument("-o", required=True); p.add_argument("--models")
    p = sub.add_parser("run"); p.add_argument("plan"); p.add_argument("--results", required=True); p.add_argument("--jobs", required=True)
    p.add_argument("--budget", type=float, required=True); p.add_argument("--start-balance", type=float); p.add_argument("-j", type=int, default=4)
    p.add_argument("--judge-env", help="JSON file: {source: {VAR: value}} passed to judged verifiers")
    args = ap.parse_args()
    {"inventory": inventory, "label": label, "select": select, "plan": plan, "run": run}[args.cmd](args)
    return 0


if __name__ == "__main__":
    sys.exit(main())
