#!/usr/bin/env bash
# Smoke test: the local web SQL console (`duckdb-host ui`). The native host owns
# the listening socket (httplib can't listen inside the wasip2 sandbox) and
# bridges queries to the core component, returning JSON. Fully offline.
set -euo pipefail
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
cd "$ROOT"

HOST=./target/release/duckdb-host
[[ -x "$HOST" ]] || cargo build --release -p duckdb-component-host --bin duckdb-host

PORT=4291
"$HOST" ui --no-open --port "$PORT" >/tmp/smoke-ui.log 2>&1 &
SRV=$!
trap 'kill $SRV 2>/dev/null || true' EXIT
sleep 7

echo "=== GET / serves the console ==="
curl -s -m 10 "http://127.0.0.1:$PORT/" | grep -oE 'SQL console' | head -1

echo "=== POST /api/query: typed JSON result ==="
curl -s -m 30 -X POST "http://127.0.0.1:$PORT/api/query" \
  --data "SELECT 42 AS answer, 'hi' AS greeting, NULL AS n;"
echo ""

echo "=== extensions visible through the console ==="
curl -s -m 30 -X POST "http://127.0.0.1:$PORT/api/query" \
  --data "SELECT count(*) AS loaded FROM duckdb_extensions() WHERE loaded;"
echo ""

echo "=== error path returns a clean JSON error ==="
curl -s -m 15 -X POST "http://127.0.0.1:$PORT/api/query" --data "SELECT * FROM nope;" \
  | grep -oiE '"error":"Catalog Error[^"]*' | head -c 60
echo ""
