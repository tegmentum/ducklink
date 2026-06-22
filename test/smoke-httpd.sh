#!/usr/bin/env bash
# Smoke test: duckdb-wasm-httpd (`ducklink serve`). The native host owns the
# listening socket and runs SQL through the wasm DuckDB core, returning JSON.
# A port of sqlite-wasm-httpd: built-in /health|/sql|/tables|/schema endpoints
# plus a database-driven `routes` table (kind = sql|static|blob|wasm).
set -euo pipefail
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
cd "$ROOT"

HOST=./target/release/ducklink
[[ -x "$HOST" ]] || cargo build --release -p ducklink-host --bin ducklink

# Build the reference request-handler component (kind='wasm' dispatch target).
HANDLER=target/wasm32-wasip2/release/echo_handler.wasm
[[ -f "$HANDLER" ]] || cargo component build -p echo-handler --target wasm32-wasip2 --release

PORT=8137
"$HOST" serve --init-routes --port "$PORT" --load echo="$HANDLER" >/tmp/smoke-httpd.log 2>&1 &
SRV=$!
trap 'kill "$SRV" 2>/dev/null || true' EXIT

# wait for listen
for _ in $(seq 1 20); do
  sleep 3
  curl -s -m 3 -o /dev/null "http://127.0.0.1:$PORT/health" 2>/dev/null && break
done

base="http://127.0.0.1:$PORT"
post() { curl -s -m 10 -X POST "$base/sql" --data "$1" >/dev/null; }
pass() { echo "PASS: $1"; }
fail() { echo "FAIL: $1"; FAILED=1; }
FAILED=0

echo "=== built-in endpoints ==="
[[ "$(curl -s -m 10 "$base/health")" == "ok" ]] && pass "/health" || fail "/health"

r=$(curl -s -m 10 -X POST "$base/sql" --data "SELECT 42 AS answer")
[[ "$r" == '{"columns":["answer"],"rows":[[42]],"rowcount":1}' ]] && pass "POST /sql" || fail "POST /sql ($r)"

r=$(curl -s -m 10 "$base/sql?q=SELECT%201%2B1%20AS%20two")
echo "$r" | grep -q '"two"' && pass "GET /sql?q=" || fail "GET /sql?q= ($r)"

post "CREATE TABLE t(a INTEGER, b VARCHAR)"
curl -s -m 10 "$base/tables" | grep -q '"t"' && pass "/tables" || fail "/tables"
curl -s -m 10 "$base/schema/t" | grep -q '"name"' && pass "/schema/t" || fail "/schema/t"

echo "=== db-driven router ==="
# seeded /hello (no-param SQL handler)
[[ "$(curl -s -m 10 "$base/hello")" == "{}" ]] && pass "seeded /hello" || fail "/hello"

# sql kind, named params ($body, $path) — subset, reordered
post "INSERT INTO routes (method,pattern,handler,kind,ctype) VALUES ('POST','/echo/*','SELECT \$body AS got_body, \$path AS got_path','sql','application/json')"
r=$(curl -s -m 10 -X POST "$base/echo/abc" --data "hi")
echo "$r" | grep -q '"got_body":"hi"' && echo "$r" | grep -q '"got_path":"/echo/abc"' \
  && pass "sql named params" || fail "sql named params ($r)"

# single-column value IS the body, via $body
post "INSERT INTO routes (method,pattern,handler,ctype) VALUES ('POST','/upper','SELECT upper(\$body) AS body','text/plain')"
[[ "$(curl -s -m 10 -X POST "$base/upper" --data "hello world")" == "HELLO WORLD" ]] \
  && pass "sql body override" || fail "sql body override"

# status/ctype override columns
post "INSERT INTO routes (method,pattern,handler,kind) VALUES ('GET','/teapot','SELECT 418 AS status, ''text/plain'' AS ctype, ''teapot'' AS body','sql')"
code=$(curl -s -m 10 -o /dev/null -w '%{http_code}' "$base/teapot")
[[ "$code" == 418 ]] && pass "sql status override" || fail "sql status override ($code)"

# static kind
post "INSERT INTO routes (method,pattern,handler,kind,ctype) VALUES ('GET','/v','{\"v\":1}','static','application/json')"
[[ "$(curl -s -m 10 "$base/v")" == '{"v":1}' ]] && pass "static kind" || fail "static kind"

# blob kind
post "INSERT INTO routes (method,pattern,handler,kind) VALUES ('GET','/b','SELECT ''abc''::BLOB AS body','blob')"
[[ "$(curl -s -m 10 "$base/b")" == "abc" ]] && pass "blob kind" || fail "blob kind"

# wasm kind -> dispatch to the loaded echo handler component
post "INSERT INTO routes (method,pattern,handler,kind) VALUES ('*','/echo','echo','wasm')"
r=$(curl -s -m 15 -X POST "$base/echo" --data "ping")
echo "$r" | grep -q 'echo:' && echo "$r" | grep -q '"text":"ping"' \
  && pass "wasm kind dispatch" || fail "wasm kind dispatch ($r)"

# wasm route naming an unknown handler -> 500
post "INSERT INTO routes (method,pattern,handler,kind) VALUES ('GET','/w','nope','wasm')"
code=$(curl -s -m 10 -o /dev/null -w '%{http_code}' "$base/w")
[[ "$code" == 500 ]] && pass "wasm unknown handler -> 500" || fail "wasm unknown handler ($code)"

# unknown -> 404
code=$(curl -s -m 10 -o /dev/null -w '%{http_code}' "$base/nope")
[[ "$code" == 404 ]] && pass "404 unknown" || fail "404 unknown ($code)"

echo ""
[[ "$FAILED" == 0 ]] && echo "ALL PASS" || { echo "SOME FAILED"; exit 1; }
