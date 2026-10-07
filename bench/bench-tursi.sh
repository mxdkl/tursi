#!/usr/bin/env bash
# Harness-Bench generic_cli adapter wrapper for tursi.
#
# Harness-Bench invokes this as:  bench-tursi.sh <prompt_file> <workspace>
# with cwd already set to <workspace> and these env vars available:
#   HARNESSBENCH_TASK_ID, HARNESSBENCH_WORKSPACE,
#   HARNESSBENCH_LLM_PROXY_URL, HARNESSBENCH_LLM_PROXY_ROUTES
#
# tursi uses the project working directory as its project root, so running
# here makes <workspace> the project. --no-sandbox keeps edits on the real
# files the oracle will grade. --json prints a one-line usage/cost summary
# that we tee to tursi-result.json in the workspace for later collection.
set -euo pipefail

PROMPT_FILE="${1:?prompt_file arg required}"
WORKSPACE="${2:-$PWD}"

# Locate the tursi binary. Allow override; otherwise use the release build
# next to this script's repo.
TURSI_BIN="${TURSI_BIN:-}"
if [ -z "$TURSI_BIN" ]; then
  SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
  TURSI_BIN="$SCRIPT_DIR/../target/release/tursi"
fi

cd "$WORKSPACE"

PROMPT="$(cat "$PROMPT_FILE")"

# Extra tursi arguments for this harness entry, e.g. TURSI_ARGS="--pool <model>"
# for a single-model baseline (bench-tursi-glm.sh).
read -r -a EXTRA <<< "${TURSI_ARGS:-}"

# Emit the JSON summary to a sidecar file in the workspace and to stdout.
"$TURSI_BIN" --task "$PROMPT" --no-sandbox --json "${EXTRA[@]}" | tee "$WORKSPACE/tursi-result.json"
