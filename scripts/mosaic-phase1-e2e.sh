#!/usr/bin/env bash
# Mosaic Phase 1 end-to-end smoke.
#
# Proves the full Phase 1 acceptance criterion (design memo §27) via the
# direct-nested-exec path (ADR Decision 6 + Phase 4 shared-manager
# sibling — the `_install_sql`/POST workaround is gone):
#   1. `ducklink` CLI (mosaic extension loaded) opens a disk-backed DB,
#      seeds a fixture table + the `routes` table, then invokes
#      `mosaic_create(name, spec, opts)`. mosaic_create's own body
#      nested-execs the __mosaic_apps + routes INSERTs from inside the
#      scalar (sibling connection against the primary DB); returns the
#      app URL.
#   2. `ducklink serve` starts on the same DB (routes already installed).
#   3. Curl the shell + bundle + spec route + the Mosaic REST endpoint,
#      with + without the token, and assert the responses.
#
# Usage:
#   bash scripts/mosaic-phase1-e2e.sh                 # port 18789
#   PORT=9010 bash scripts/mosaic-phase1-e2e.sh       # override port
#   KEEP_ALIVE=1 bash scripts/mosaic-phase1-e2e.sh    # leave serve up

set -euo pipefail

PORT="${PORT:-18789}"
REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
WORK_DIR="$(mktemp -d -t ducklink-mosaic-p1-XXXXXX)"
DB_FILE="mosaic-e2e.duckdb"

DUCKLINK_BIN="${DUCKLINK_BIN:-$REPO_ROOT/target/release/ducklink}"
EXT_DIR="${EXT_DIR:-$REPO_ROOT/artifacts/extensions}"

if [[ ! -x "$DUCKLINK_BIN" ]]; then
  echo "ducklink not built at $DUCKLINK_BIN" >&2
  echo "run: (cd $REPO_ROOT && cargo build --release -p ducklink-host --bin ducklink)" >&2
  exit 1
fi
if [[ ! -f "$EXT_DIR/mosaic.wasm" ]]; then
  echo "mosaic extension not staged at $EXT_DIR/mosaic.wasm" >&2
  echo "run: make -C $REPO_ROOT mosaic" >&2
  exit 1
fi

APP_NAME="demo"
TOKEN="deadbeefdeadbeefdeadbeefdeadbeef"
SPEC='{"data":{"fixture":{"query":"SELECT * FROM fixture"}},"width":720,"height":320,"plot":[{"mark":"line","from":"fixture","x":"ts","y":"v"}]}'
OPTS="{\"token\":\"$TOKEN\"}"

echo ">>> workdir: $WORK_DIR"
cd "$WORK_DIR"

# 1) Seed fixtures + install the mosaic app via the CLI (single ducklink
#    invocation, writes to the on-disk DB the server picks up in step 2).
#    Both the `routes` DDL and the `mosaic_create` INSERTs commit before
#    the CLI exits — no `serve --init-routes`, no POST /sql plumbing.
echo ">>> installing mosaic app via ducklink CLI (mosaic_create direct)"
"$DUCKLINK_BIN" --extensions-dir "$EXT_DIR" -- "$DB_FILE" --load-extension mosaic 2>cli.err <<EOF > cli.out
.header off
LOAD mosaic;
CREATE TABLE IF NOT EXISTS routes (
    method   VARCHAR NOT NULL,
    pattern  VARCHAR NOT NULL,
    handler  VARCHAR NOT NULL,
    kind     VARCHAR NOT NULL DEFAULT 'sql',
    status   INTEGER DEFAULT 200,
    ctype    VARCHAR,
    priority INTEGER DEFAULT 0
);
CREATE TABLE fixture AS
  SELECT (DATE '2026-01-01' + INTERVAL (i) DAY) AS ts,
         (30 + 10*sin(i*0.5))::DOUBLE          AS v
  FROM range(20) t(i);
SELECT mosaic_create('$APP_NAME', '$SPEC', '$OPTS') AS url;
EOF

if ! grep -q "/ducklink/mosaic/app/$APP_NAME" cli.out; then
  echo "!!! mosaic_create did not return an app URL" >&2
  echo "    cli.out:"    >&2; sed 's/^/      /' cli.out >&2
  echo "    cli.err:"    >&2; sed 's/^/      /' cli.err >&2
  exit 1
fi

# 2) Start ducklink serve on the same DB. Routes are already there — no
#    --init-routes needed (the CLI step created the table).
echo ">>> starting ducklink serve on :$PORT"
"$DUCKLINK_BIN" serve --db "$DB_FILE" --port "$PORT" 2>server.log &
SERVER_PID=$!
trap 'kill "$SERVER_PID" 2>/dev/null || true; wait 2>/dev/null || true; echo ">>> stopped (logs at $WORK_DIR/server.log)"' EXIT

for _ in $(seq 1 40); do
  if curl -sf -m 1 "http://127.0.0.1:$PORT/health" >/dev/null; then break; fi
  sleep 0.1
done
if ! curl -sf -m 1 "http://127.0.0.1:$PORT/health" >/dev/null; then
  echo "!!! ducklink did not come up — see $WORK_DIR/server.log" >&2
  tail -20 server.log >&2 || true
  exit 1
fi
echo ">>> up"

# 3) Assertions.
FAIL=0
assert_http_status() {
  local expected="$1" method="$2" url="$3" extra="$4"
  local got
  got=$(curl -s -o /dev/null -w '%{http_code}' -X "$method" $extra "$url" || true)
  if [[ "$got" != "$expected" ]]; then
    echo "!!! FAIL: expected $expected got $got for $method $url" >&2
    FAIL=1
  else
    echo "    OK  $method $url -> $got"
  fi
}
assert_body_contains() {
  local needle="$1" url="$2"
  local body
  body=$(curl -s "$url" || true)
  if [[ "$body" != *"$needle"* ]]; then
    echo "!!! FAIL: body of $url did not contain $needle" >&2
    echo "    body[0..200]=${body:0:200}" >&2
    FAIL=1
  else
    echo "    OK  body-contains($needle) in $url"
  fi
}

echo ">>> asserting the SPA shell (index.html)"
assert_http_status 200 GET "http://127.0.0.1:$PORT/ducklink/mosaic/app/$APP_NAME" ""
assert_body_contains "mosaicRuntime.boot" "http://127.0.0.1:$PORT/ducklink/mosaic/app/$APP_NAME"

echo ">>> asserting the browser bundle (bundle.js)"
assert_http_status 200 GET "http://127.0.0.1:$PORT/ducklink/mosaic/app/$APP_NAME/bundle.js" ""
BUNDLE_LEN=$(curl -s "http://127.0.0.1:$PORT/ducklink/mosaic/app/$APP_NAME/bundle.js" | wc -c | tr -d ' ')
echo "    bundle.js length = $BUNDLE_LEN bytes"
if [[ "$BUNDLE_LEN" -lt 100000 ]]; then
  echo "!!! FAIL: bundle.js is suspiciously small (${BUNDLE_LEN} bytes)" >&2
  FAIL=1
fi

echo ">>> asserting the spec endpoint"
assert_body_contains '"query"' "http://127.0.0.1:$PORT/ducklink/mosaic/api/app/$APP_NAME/spec"

echo ">>> asserting the query endpoint (with token) returns row objects"
QUERY_BODY='{"type":"json","sql":"SELECT * FROM fixture ORDER BY ts LIMIT 3"}'
ANS_OK=$(curl -s -X POST -H "Content-Type: application/json" \
  --data-binary "$QUERY_BODY" \
  "http://127.0.0.1:$PORT/ducklink/mosaic/api/app/$APP_NAME/query?token=$TOKEN" || true)
echo "    query answer[0..160]=${ANS_OK:0:160}"
if [[ "$ANS_OK" != *'"ts"'* ]]; then
  echo "!!! FAIL: /api/app/$APP_NAME/query with token did not return row objects with 'ts' column" >&2
  FAIL=1
fi

echo ">>> asserting the query endpoint (without token) returns UNAUTHORIZED"
ANS_NO=$(curl -s -X POST -H "Content-Type: application/json" \
  --data-binary "$QUERY_BODY" \
  "http://127.0.0.1:$PORT/ducklink/mosaic/api/app/$APP_NAME/query" || true)
echo "    unauthorized answer[0..160]=${ANS_NO:0:160}"
if [[ "$ANS_NO" != *'UNAUTHORIZED'* ]]; then
  echo "!!! FAIL: /api/app/$APP_NAME/query without token did not return UNAUTHORIZED (got $ANS_NO)" >&2
  FAIL=1
fi

if [[ "$FAIL" -eq 0 ]]; then
  echo ">>> ALL ASSERTIONS PASSED."
  echo ">>> Open http://127.0.0.1:$PORT/ducklink/mosaic/app/$APP_NAME?token=$TOKEN"
  if [[ "${KEEP_ALIVE:-0}" == "1" ]]; then
    echo ">>> Server still running (PID $SERVER_PID). Ctrl-C to stop."
    wait "$SERVER_PID"
  fi
  exit 0
else
  echo ">>> FAILURES above. See $WORK_DIR/server.log"
  exit 1
fi
