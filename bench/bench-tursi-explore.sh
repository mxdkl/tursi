#!/usr/bin/env bash
# Harness-Bench wrapper for a training run: the whole pool, with coverage on
# (every station gets at least 5 tasks before scores decide, §5.8), so each
# model earns a record across the task spread.
set -euo pipefail
export TURSI_ARGS="--explore-min ${EXPLORE_MIN:-5}"
exec "$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)/bench-tursi.sh" "$@"
