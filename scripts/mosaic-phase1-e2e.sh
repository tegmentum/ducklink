#!/usr/bin/env bash
# Mosaic Phase 1 end-to-end smoke.
#
# Proves the full Phase 1 acceptance criterion (design memo §27):
#   1. `ducklink serve` starts up with a routes table.
#   2. `ducklink` CLI (mosaic extension loaded) builds the install SQL
#      via mosaic_install_sql(...). We use the workaround scalar because
#      `mosaic_create` needs nested-exec re-entry into the outer
#      connection — a pre-existing standalone-CLI limitation the
#      fieldbook_* scalars also hit today (docs/mosaic-phase0-findings.md
#      §1.2 + docs/nested-exec-direction-1-plan.md §7.4). The SQL
#      mosaic_install_sql returns is byte-identical to what
#      mosaic_create would execute internally.
#   3. Curl POST /sql installs the routes.
#   4. Curl the shell + bundle + spec route + the Mosaic REST endpoint,
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
if ! command -v python3 >/dev/null 2>&1; then
  echo "python3 not on PATH (needed to strip CLI prompts from the JSON output)" >&2
  exit 1
fi

APP_NAME="demo"
TOKEN="deadbeefdeadbeefdeadbeefdeadbeef"
SPEC='{"data":{"fixture":{"query":"SELECT * FROM fixture"}},"width":720,"height":320,"plot":[{"mark":"line","from":"fixture","x":"ts","y":"v"}]}'
OPTS="{\"token\":\"$TOKEN\"}"

echo ">>> workdir: $WORK_DIR"
cd "$WORK_DIR"

# 1) Start ducklink serve. Uses a fresh disk-backed DB so `POST /sql`
#    changes persist across the multiple curl calls below.
echo ">>> starting ducklink serve on :$PORT"
"$DUCKLINK_BIN" serve --db "$DB_FILE" --port "$PORT" --init-routes 2>server.log &
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

post_sql() {
  local sql="$1"
  local out
  out=$(curl -sf -X POST "http://127.0.0.1:$PORT/sql" --data-binary "$sql") || {
    echo "!!! SQL failed: $sql" >&2
    exit 1
  }
  echo "$out"
}

# 2) Seed a fixture table so the spec's query has data to render.
echo ">>> seeding fixture table"
post_sql "CREATE TABLE fixture AS
          SELECT (DATE '2026-01-01' + INTERVAL (i) DAY) AS ts,
                 (30 + 10*sin(i*0.5))::DOUBLE          AS v
          FROM range(20) t(i)" >/dev/null

# 3) Build the install SQL via the ducklink CLI + mosaic_install_sql.
#    Runs against a throwaway :memory: DB — mosaic_install_sql is pure
#    (no nested-exec, no filesystem, no network), so it's safe to run
#    without an on-disk DB.
echo ">>> building install SQL (ducklink CLI + mosaic_install_sql)"
"$DUCKLINK_BIN" --extensions-dir "$EXT_DIR" -- :memory: --load-extension mosaic 2>cli.err <<EOF > cli.json
.mode json
.header off
LOAD mosaic;
SELECT mosaic_install_sql('$APP_NAME', '$SPEC', '$OPTS') AS s;
EOF

# Strip DuckDB CLI prompts (D> etc.) from the JSON output. Python writes
# `install.sql` directly (its stdout print goes to the console via the
# script's stderr).
python3 - <<'PY'
import json, pathlib, sys
raw = pathlib.Path("cli.json").read_text()
# The JSON row shape is [{"s":"..."}]. Find its span; ignore the CLI's
# prompt echo + the initial LOAD mosaic "success" table.
start = raw.find('[{"s":')
end = raw.rfind('"}]') + 3
if start < 0 or end < start:
    raise SystemExit(f"could not find [{{\"s\": ... }}] in CLI output:\n{raw[:400]}")
data = json.loads(raw[start:end])
pathlib.Path("install.sql").write_text(data[0]["s"])
print(f"    install SQL bytes: {len(data[0]['s'])}", file=sys.stderr)
PY

# 4) POST /sql to install the routes. The batch includes the
#    __mosaic_apps CREATE + INSERT, the shared /api/query route, and the
#    three per-app routes (index.html static, bundle.js static, spec sql).
echo ">>> installing routes via POST /sql"
INSTALL_RESP=$(curl -s -w '\nHTTP_CODE=%{http_code}' -X POST "http://127.0.0.1:$PORT/sql" --data-binary @install.sql)
INSTALL_CODE=${INSTALL_RESP##*HTTP_CODE=}
INSTALL_BODY=${INSTALL_RESP%HTTP_CODE=*}
echo "    install response code: $INSTALL_CODE"
echo "    install response body[0..200]: ${INSTALL_BODY:0:200}"
if [[ "$INSTALL_CODE" != "200" ]]; then
  echo "!!! FAIL: install POST /sql returned $INSTALL_CODE" >&2
  exit 1
fi

# 5) Assertions.
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
