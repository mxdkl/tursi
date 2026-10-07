#!/usr/bin/env python3
"""Summarize Harness-Bench results for tursi.

Walks a Harness-Bench results directory, reads each task's combined_score
and the tursi cost line the wrapper left in adapter_results[].stdout, and
prints a plain-text report plus the headline dollars-per-solved-task number.

Usage:
  bench/collect.py <results_dir> [--solved-threshold 1.0]

<results_dir> is Harness-Bench's results_dir, e.g.
  <harness-bench>/data_try6/results/tursi-cf
Both a per-harness dir and its parent (many harnesses) work; it just walks
every *.json it finds.
"""
from __future__ import annotations

import argparse
import json
import sys
from pathlib import Path


def _tursi_usage(result: dict) -> dict | None:
    """Pull the tursi JSON summary the wrapper printed, from any adapter result."""
    candidates = list(result.get("adapter_results") or [])
    ar = result.get("adapter_result")
    if ar:
        candidates.append(ar)
    for item in reversed(candidates):
        stdout = (item or {}).get("stdout") or ""
        for line in reversed(stdout.splitlines()):
            line = line.strip()
            if not line.startswith("{"):
                continue
            try:
                row = json.loads(line)
            except json.JSONDecodeError:
                continue
            if "cost_usd" in row or "model_calls" in row:
                return row
    return None


def main() -> int:
    ap = argparse.ArgumentParser()
    ap.add_argument("results_dir", help="Harness-Bench results dir to walk")
    ap.add_argument("--solved-threshold", type=float, default=1.0,
                    help="combined_score at or above this counts as solved (default 1.0)")
    args = ap.parse_args()

    root = Path(args.results_dir)
    if not root.exists():
        print(f"no such results dir: {root}", file=sys.stderr)
        return 2

    rows = []
    for jf in sorted(root.rglob("*.json")):
        try:
            d = json.loads(jf.read_text(encoding="utf-8"))
        except (json.JSONDecodeError, OSError):
            continue
        if "task_id" not in d or "scoring" not in d:
            continue
        scoring = d.get("scoring") or {}
        oracle = d.get("oracle_result") or {}
        usage = _tursi_usage(d) or {}
        rows.append({
            "task": d["task_id"],
            "harness": d.get("model_id", "?"),
            "combined": scoring.get("combined_score"),
            "outcome": oracle.get("outcome_score"),
            "cost": usage.get("cost_usd"),
            "calls": usage.get("model_calls"),
            "wall_ms": usage.get("wall_ms"),
            "result": usage.get("outcome"),
        })

    if not rows:
        print(f"no result files under {root}")
        return 1

    def fmt(v, nd=4):
        return "n/a" if v is None else f"{v:.{nd}f}"

    print(f"tursi harness-bench results  ({len(rows)} task runs)")
    print("=" * 60)
    for r in rows:
        solved = r["combined"] is not None and r["combined"] >= args.solved_threshold
        mark = "PASS" if solved else "fail"
        cost = "$" + fmt(r["cost"], 5) if r["cost"] is not None else "$?"
        wall = f"{r['wall_ms']/1000:.1f}s" if r["wall_ms"] else "?"
        print(f"  [{mark}] {r['task']:34s} score={fmt(r['combined'],3)} "
              f"{cost:>9s} calls={r['calls'] or '?':>3} {wall:>6s}")

    scored = [r for r in rows if r["combined"] is not None]
    solved = [r for r in scored if r["combined"] >= args.solved_threshold]
    costs = [r["cost"] for r in rows if r["cost"] is not None]
    total_cost = sum(costs)
    solved_cost = sum(r["cost"] for r in solved if r["cost"] is not None)

    print("=" * 60)
    print(f"  tasks run       {len(rows)}")
    print(f"  scored          {len(scored)}")
    print(f"  solved          {len(solved)}  (combined_score >= {args.solved_threshold})")
    if scored:
        mean_score = sum(r['combined'] for r in scored) / len(scored)
        print(f"  mean score      {mean_score:.3f}")
    print(f"  total cost      ${total_cost:.5f}")
    if costs:
        print(f"  mean cost/task  ${total_cost/len(costs):.5f}")
    if solved:
        print(f"  cost of solved  ${solved_cost:.5f}")
        print(f"  $ / solved      ${solved_cost/len(solved):.5f}")
    else:
        print("  $ / solved      n/a (nothing solved)")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
