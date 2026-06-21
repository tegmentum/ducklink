#!/usr/bin/env bash
# Smoke test: the DuckDB UI on wasm (`ducklink ui`). The native host owns the
# listening socket (httplib can't listen() inside the wasip2 sandbox) and bridges
# each request to the core component, where the genuine duckdb-ui handlers run.
set -euo pipefail
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
cd "$ROOT"

HOST=./target/release/ducklink
[[ -x "$HOST" ]] || cargo build --release -p duckdb-component-host --bin ducklink

run() { # mode port
  local mode="$1" port="$2"
  "$HOST" ui "$mode" --no-open --port "$port" >/tmp/smoke-ui-$port.log 2>&1 &
  echo $!
}

echo "=== console mode: query -> JSON ==="
SRV=$(run --console 4271); sleep 8
curl -s -m 30 -X POST "http://127.0.0.1:4271/api/query" --data "SELECT 42 AS answer;"
echo ""
kill "$SRV" 2>/dev/null || true

echo "=== offline mode: real SPA assets + bridged /ddb/run ==="
SRV=$(run --offline 4272); sleep 8
curl -s -m 10 "http://127.0.0.1:4272/" | grep -oqE 'hatchling\.[0-9a-f]+\.bundle\.js' \
  && echo "PASS: real index.html served (bundle reference present)" || echo "FAIL: index"
sz=$(curl -s -m 25 -o /dev/null -w '%{size_download}' "http://127.0.0.1:4272/$(curl -s http://127.0.0.1:4272/ | grep -oE 'hatchling\.[0-9a-f]+\.bundle\.js' | head -1)")
echo "  bundle.js: $sz bytes"
# /ddb/run returns DuckDB's BinarySerializer format (binary, contains the col name)
code=$(curl -s -m 20 -X POST "http://127.0.0.1:4272/ddb/run" -H 'X-DuckDB-UI-Connection-Name: c1' \
  --data 'SELECT 42 AS answer' -o /tmp/smoke-run -w '%{http_code}')
if [[ "$code" == 200 ]] && grep -q answer /tmp/smoke-run; then
  echo "PASS: /ddb/run bridged -> BinarySerializer (200, contains 'answer')"
else
  echo "FAIL: /ddb/run (code=$code)"
fi
kill "$SRV" 2>/dev/null || true
