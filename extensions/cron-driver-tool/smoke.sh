#!/usr/bin/env bash
# End-to-end smoke test for the cron-driver-tool wasm component.
#
# cron-driver-tool is a `wasi:cli/run` tool (not a `duckdb:extension`), so it
# cannot be exercised through the standard `tooling/smoke.py` extension harness.
# This standalone script drives it via `ducklink cron run --wasm-driver`.
#
# NOTE: this script sleeps ~65s so that a minute-boundary passes and the
# `* * * * *` schedule becomes due before we tick the loop.
set -euo pipefail

# 1. cd to repo root so relative paths resolve.
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/../.." && pwd)"
cd "$REPO_ROOT"

DUCKLINK_BIN="target/release/ducklink"
WASM_ARTIFACT="artifacts/extensions/cron_driver_tool.wasm"

if [[ ! -x "$DUCKLINK_BIN" ]]; then
  echo "smoke: missing $DUCKLINK_BIN -- build it with: make host" >&2
  exit 1
fi
if [[ ! -f "$WASM_ARTIFACT" ]]; then
  echo "smoke: missing $WASM_ARTIFACT -- build it with: make cron-driver" >&2
  exit 1
fi

TMPDIR_SMOKE="$(mktemp -d)"
trap 'rm -rf "$TMPDIR_SMOKE"' EXIT
DB="$TMPDIR_SMOKE/cron-smoke.duckdb"

echo "smoke: db=$DB (sleeping ~65s to cross a minute boundary)"

"$DUCKLINK_BIN" cron init --db "$DB"
"$DUCKLINK_BIN" cron schedule --db "$DB" 'smoke-job' '* * * * *' 'CHECKPOINT'

sleep 65

RUN_ERR="$TMPDIR_SMOKE/run.stderr"
if ! "$DUCKLINK_BIN" cron run --db "$DB" --wasm-driver --once 2>"$RUN_ERR"; then
  echo "smoke: cron run --wasm-driver --once failed" >&2
  cat "$RUN_ERR" >&2
  exit 1
fi

if ! grep -q 'fired 1 job(s)' "$RUN_ERR"; then
  echo "smoke: expected 'fired 1 job(s)' in stderr; got:" >&2
  cat "$RUN_ERR" >&2
  exit 1
fi

if ! "$DUCKLINK_BIN" cron list --db "$DB" | grep -q '"last_status":"fired"'; then
  echo "smoke: last_status was not updated to 'fired' in cron list output" >&2
  exit 1
fi

echo "OK"
