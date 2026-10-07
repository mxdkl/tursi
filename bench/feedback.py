#!/usr/bin/env python3
"""Ground truth for DEALS (SPEC §5.8): append each graded Harness-Bench task's
oracle score to ~/.tursi/deals/outcomes.jsonl as a `truth` record for the
workspace it ran in. The pool then learns from the score instead of its QA
judge's guess for every outcome of that task (the judge's value stays in the
log, for calibrating the judge). Appends only, so it is safe while tursi runs;
a workspace that already has a truth is skipped.

  bench/feedback.py <results_dir> [<results_dir> ...]
"""
import datetime, glob, json, os, sys

OUTCOMES = os.path.expanduser("~/.tursi/deals/outcomes.jsonl")


def main(dirs):
    have = set()
    if os.path.exists(OUTCOMES):
        for line in open(OUTCOMES):
            try:
                rec = json.loads(line)
            except ValueError:
                continue
            if "truth" in rec:
                have.add(rec["project"].rstrip("/"))
    added = []
    for d in dirs:
        for f in sorted(glob.glob(os.path.join(d, "**", "*.json"), recursive=True)):
            try:
                r = json.load(open(f))
            except (OSError, ValueError):
                continue
            ws = (r.get("workspace") or "").rstrip("/")
            score = (r.get("scoring") or {}).get("combined_score")
            if not ws or score is None or ws in have:
                continue
            rec = {
                "ts": datetime.datetime.now(datetime.timezone.utc).isoformat(),
                "project": ws,
                "truth": max(0.0, min(1.0, float(score))),
                "source": f"harness-bench:{r.get('task_id') or os.path.basename(f)[:-5]}:{r.get('model_id', '')}",
            }
            added.append(rec)
            have.add(ws)
    if added:
        with open(OUTCOMES, "a") as out:
            for rec in added:
                out.write(json.dumps(rec) + "\n")
    print(f"feedback: {len(added)} truth record(s) appended")


if __name__ == "__main__":
    if len(sys.argv) < 2:
        sys.exit(__doc__)
    main(sys.argv[1:])
