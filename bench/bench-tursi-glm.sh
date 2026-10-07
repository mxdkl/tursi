#!/usr/bin/env bash
# Harness-Bench wrapper for the single-model baseline: tursi's pipeline with
# the DEALS pool limited to GLM-5.3-Flash, so every task runs on it. Compare
# against bench-tursi.sh (the whole pool) on $/solved.
set -euo pipefail
export TURSI_ARGS="--pool cloudflare/@cf/zai-org/glm-5.3-flash"
exec "$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)/bench-tursi.sh" "$@"
