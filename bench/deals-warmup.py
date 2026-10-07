#!/usr/bin/env python3
"""DEALS warm-up (SPEC §5.8, the paper's warm-up split): run small graded
tasks on DEALS stations and append the verified outcomes to
~/.tursi/deals/outcomes.jsonl, the log the pool learns success rates from.

Each fixture in bench/warmup/<name>/ has meta.json (the labels tursi's intake
gives its prompt: activity, domain, 0-4 difficulty; the prompt; protected
files), files/ (the project), and check.sh (exit 0 = solved; $REPORT is the
run's final report, $FIXTURE the fixture dir). Every run is
`tursi <copy> --task <prompt> --station <model> --json`: the model alone,
full tools, DEALS off.

  bench/deals-warmup.py --list
  bench/deals-warmup.py --stations glm-4.7-flash,gpt-oss-20b --budget 0.30
  bench/deals-warmup.py --cheap --fixtures debug-ledger,review-cache --dry-run
"""
import argparse, concurrent.futures as cf, datetime, hashlib, json, os, shutil, subprocess, sys, tempfile, threading, time

HERE = os.path.dirname(os.path.abspath(__file__))
FIXTURES = os.path.join(HERE, "warmup")
DEALS = os.path.expanduser("~/.tursi/deals")
OUTCOMES = os.path.join(DEALS, "outcomes.jsonl")


def short(model):
    return model.rsplit("/", 1)[-1]


def stations():
    try:
        cat = json.load(open(os.path.join(DEALS, "stations.json")))
    except FileNotFoundError:
        sys.exit("no station catalog — run `tursi --stations probe` first")
    return [s for s in cat["stations"] if (s.get("probe") or {}).get("ok")]


def fixtures():
    out = {}
    for name in sorted(os.listdir(FIXTURES)):
        d = os.path.join(FIXTURES, name)
        if os.path.isfile(os.path.join(d, "meta.json")):
            out[name] = {**json.load(open(os.path.join(d, "meta.json"))), "dir": d}
    return out


def digest(path):
    return hashlib.sha256(open(path, "rb").read()).hexdigest() if os.path.exists(path) else None


def run_one(tursi, model, name, fx, max_turns, timeout, keep=False):
    work = tempfile.mkdtemp(prefix=f"tursi-warmup-{name}-")
    shutil.copytree(os.path.join(fx["dir"], "files"), work, dirs_exist_ok=True)
    # Pin the project root here: tursi takes the nearest ancestor holding a
    # .tursi/ as the project, and a stray /tmp/.tursi would swallow every run.
    os.makedirs(os.path.join(work, ".tursi"), exist_ok=True)
    protected = {p: digest(os.path.join(work, p)) for p in fx.get("protect", [])}
    started = time.time()
    cmd = [tursi, work, "--task", fx["prompt"], "--station", model, "--json", "--max-turns", str(max_turns)]
    try:
        p = subprocess.run(cmd, capture_output=True, text=True, timeout=timeout)
        out = p.stdout
    except subprocess.TimeoutExpired as e:
        out = (e.stdout or b"").decode() if isinstance(e.stdout, bytes) else (e.stdout or "")
    secs = int(time.time() - started)
    result = {}
    for line in reversed(out.strip().splitlines()):
        try:
            result = json.loads(line)
            break
        except ValueError:
            continue
    report = os.path.join(work, ".report.txt")
    open(report, "w").write(result.get("summary") or "")
    tampered = [p for p, h in protected.items() if digest(os.path.join(work, p)) != h]
    check = subprocess.run(["sh", os.path.join(fx["dir"], "check.sh")], cwd=work, capture_output=True, text=True,
                           env={**os.environ, "REPORT": report, "FIXTURE": fx["dir"]}, timeout=120)
    solved = check.returncode == 0 and not tampered and bool(result)
    note = "changed protected " + ",".join(tampered) if tampered else ("no result" if not result else check.stdout.strip()[-80:])
    note = f"{result.get('model_calls', '?')} calls {note}".strip()
    if keep:
        note += f"  [{work}]"
    else:
        shutil.rmtree(work, ignore_errors=True)
    return {"solved": solved, "cost": float(result.get("cost_usd") or 0.0), "secs": secs, "note": note}


def main():
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("--stations", help="comma-separated station names (short or full ids); default: all qualified")
    ap.add_argument("--cheap", action="store_true", help="only stations at or under $0.50/M input and $1.50/M output")
    ap.add_argument("--fixtures", help="comma-separated fixture names; default: all")
    ap.add_argument("--reps", type=int, default=1)
    ap.add_argument("--jobs", type=int, default=4, help="stations run in parallel; one station runs serially")
    ap.add_argument("--budget", type=float, default=0.5, help="stop starting runs past this many dollars")
    ap.add_argument("--max-turns", type=int, default=25)
    ap.add_argument("--timeout", type=int, default=600)
    ap.add_argument("--list", action="store_true")
    ap.add_argument("--keep", action="store_true", help="keep each run's project copy (with .tursi logs)")
    ap.add_argument("--dry-run", action="store_true")
    ap.add_argument("--tursi", default=os.environ.get("TURSI_BIN", "tursi"))
    a = ap.parse_args()

    pool, fx = stations(), fixtures()
    if a.list:
        for n, f in fx.items():
            print(f"{n:20} {f['activity'] + '·' + f['domain']:18} {f['prompt'][:70]}")
        print(f"\n{len(pool)} qualified stations: {', '.join(short(s['model']) for s in pool)}")
        return
    if a.stations:
        want = {w.strip() for w in a.stations.split(",")}
        pool = [s for s in pool if s["model"] in want or short(s["model"]) in want]
    if a.cheap:
        pool = [s for s in pool if s["prices"]["input"] <= 0.5 and s["prices"]["output"] <= 1.5]
    if a.fixtures:
        want = {w.strip() for w in a.fixtures.split(",")}
        fx = {n: f for n, f in fx.items() if n in want}
    runs = [(s["model"], n) for s in pool for n in fx for _ in range(a.reps)]
    print(f"{len(runs)} runs: {len(pool)} stations × {len(fx)} fixtures × {a.reps}; budget ${a.budget:.2f}")
    if a.dry_run or not runs:
        for m, n in runs:
            print(f"  {short(m):32} {n}")
        return

    spent, lock, results = [0.0], threading.Lock(), []
    by_station = {}
    for m, n in runs:
        by_station.setdefault(m, []).append(n)

    def station_worker(model, names):
        for name in names:
            with lock:
                if spent[0] >= a.budget:
                    print(f"  budget reached — skipping {short(model)} {name}")
                    continue
            r = run_one(a.tursi, model, name, fx[name], a.max_turns, a.timeout, a.keep)
            outcome = {
                "ts": datetime.datetime.now(datetime.timezone.utc).isoformat(),
                "project": f"warmup:{name}",
                **{k: fx[name][k] for k in ("activity", "domain", "difficulty") if k in fx[name]},
                "stations": [model],
                "success": 1.0 if r["solved"] else 0.0,
                "cost": {model: r["cost"]},
                "secs": r["secs"],
            }
            with lock:
                spent[0] += r["cost"]
                with open(OUTCOMES, "a") as f:
                    f.write(json.dumps(outcome) + "\n")
                results.append((model, name, r))
                print(f"  {'✓' if r['solved'] else '✗'} {short(model):32} {name:20} ${r['cost']:.4f} {r['secs']:>4}s  {r['note']}", flush=True)

    os.makedirs(DEALS, exist_ok=True)
    with cf.ThreadPoolExecutor(a.jobs) as ex:
        list(ex.map(lambda kv: station_worker(*kv), by_station.items()))
    solved = sum(r["solved"] for _, _, r in results)
    print(f"\n{solved}/{len(results)} solved, ${spent[0]:.3f} spent; outcomes appended to {OUTCOMES}")


if __name__ == "__main__":
    main()
