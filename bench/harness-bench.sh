#!/usr/bin/env bash
# Driver for running Harness-Bench against tursi.
#
# This wraps the Harness-Bench CLI with the settings tursi benchmarking
# wants by default:
#   - oracle-only grading (skip the LLM process judge) unless you ask for it
#   - the tursi-cf harness config
#   - the cloned Harness-Bench checkout in the scratchpad
#
# Usage:
#   bench/harness-bench.sh task 001-file          # one task
#   bench/harness-bench.sh num 40                 # one task by leading number
#   bench/harness-bench.sh suite --from-num 39 --to-num 48   # a range
#   bench/harness-bench.sh collect                # summarize results so far
#
# Env overrides:
#   HB_DIR     path to the Harness-Bench checkout
#   HARNESS    harness config id (default tursi-cf)
#   PROCESS_GRADE=1   run the LLM process judge too (costs more, needs a judge model)
set -euo pipefail

HB_DIR="${HB_DIR:-/tmp/claude-1000/-home-player1-tursi-harness/ce9cfd37-37c9-4329-8434-6e2b2ffdd17d/scratchpad/impl/harness-bench}"
HARNESS="${HARNESS:-tursi-cf}"
REPO_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"

if [ ! -d "$HB_DIR" ]; then
  echo "Harness-Bench checkout not found at: $HB_DIR" >&2
  echo "Set HB_DIR, or re-clone: git clone --depth 1 https://github.com/Qihoo360/harness-bench \"\$HB_DIR\"" >&2
  exit 1
fi

cmd="${1:-}"; shift || true

if [ "$cmd" = "collect" ]; then
  python3 "$REPO_DIR/bench/collect.py" "$HB_DIR/data_try6/results/$HARNESS" "$@"
  exit $?
fi

# Everything else drives the Harness-Bench CLI inside its venv.
cd "$HB_DIR"
# shellcheck disable=SC1091
. .venv/bin/activate

if [ "${PROCESS_GRADE:-0}" != "1" ]; then
  export HARNESSBENCH_SKIP_PROCESS_GRADE=1
fi
# Browser tasks (003, 006, 078, 081, 088) want their mock site at a "public"
# URL for remote harnesses; tursi runs on this machine, so the local one does.
export HARNESSBENCH_PUBLIC_URL_TEMPLATE="${HARNESSBENCH_PUBLIC_URL_TEMPLATE:-{local_url}}"

status=0
case "$cmd" in
  task)  python -m harnessbench.cli run-task  --task "$1" --harness "$HARNESS" "${@:2}" || status=$? ;;
  num)   python -m harnessbench.cli run-task  --num  "$1" --harness "$HARNESS" "${@:2}" || status=$? ;;
  suite) python -m harnessbench.cli run-suite --harness "$HARNESS" "$@" || status=$? ;;
  tasks) python -m harnessbench.cli tasks; exit $? ;;
  *)
    echo "usage: bench/harness-bench.sh {task <id>|num <n>|suite [range]|tasks|collect}" >&2
    exit 2 ;;
esac
# The oracle's scores become ground truth for the pool (bench/feedback.py).
python3 "$REPO_DIR/bench/feedback.py" "$HB_DIR/data_try6/results/$HARNESS"
exit $status
