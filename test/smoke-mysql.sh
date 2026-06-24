#!/usr/bin/env bash
# Smoke test: the mysql_scanner extension (MariaDB Connector/C built for wasm,
# TCP+TLS over httpfs's wasip2 socket graft + openssl-wasm) loads, registers its
# catalog (mysql_query / mysql_execute + ATTACH ... TYPE mysql), exposes the
# mysql SECRET type and mysql_* SETTINGS, and that an ATTACH to an unreachable
# host dispatches into MariaDB Connector/C's connect path -- i.e. the scanner is
# fully wired through, not merely "function not found". On the THREADLESS wasm
# core, that connect aborts with "thread constructor failed: Not supported"
# (Connector/C spawns a worker thread, like postgres_scanner's libpq); see step 6.
#
# This is an OFFLINE smoke (no live MySQL/MariaDB server required), mirroring
# smoke-aws.sh: everything asserted here is deterministic without a server. To
# exercise a real round-trip, start a throwaway server first, e.g.:
#   docker run --rm -e MYSQL_ROOT_PASSWORD=pw -p 3306:3306 mysql:8
# then ATTACH 'host=host.docker.internal port=3306 database=mysql user=root
# password=pw' AS my (TYPE mysql); -- but that is NOT required to pass.
#
# REQUIREMENT TO RUN IN THE WASM CORE: rebuild the core with mysql_scanner (and
# httpfs, for the socket graft) embedded --
#   EMBED_EXTENSIONS="httpfs,mysql_scanner" ./scripts/build-libduckdb-wasm.sh
# (its libmysql_scanner_extension.a is built but not yet in the linked core).
#
# NB: the wasm CLI's `-c` renders only the first statement, so multi-statement
# demos are piped via stdin (which renders each).
set -euo pipefail
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
cd "$ROOT"

HOST=./target/release/ducklink
[[ -x "$HOST" ]] || cargo build --release -p ducklink-host --bin ducklink
DB=( "$HOST" -- duckdb-cli :memory: )

fail() { echo "FAIL: $1" >&2; exit 1; }

echo "=== 1. mysql_scanner loads + is reported loaded ==="
printf "LOAD mysql_scanner;\nSELECT extension_name, loaded FROM duckdb_extensions() WHERE extension_name='mysql_scanner';\n" \
  | "${DB[@]}" 2>/dev/null | grep -qiE 'mysql_scanner.*true' \
  || fail "mysql_scanner did not report loaded=true"
echo "PASS"

echo "=== 2. catalog: mysql_query / mysql_execute registered ==="
out=$(printf "LOAD mysql_scanner;\nSELECT function_name FROM duckdb_functions() WHERE function_name IN ('mysql_query','mysql_execute') ORDER BY 1;\n" \
  | "${DB[@]}" 2>/dev/null)
for fn in mysql_execute mysql_query; do
  echo "$out" | grep -qi "$fn" || fail "missing function: $fn"
done
echo "PASS"

echo "=== 3. mysql SECRET type registered ==="
printf "LOAD mysql_scanner;\nSELECT type FROM duckdb_secret_types() WHERE type='mysql';\n" \
  | "${DB[@]}" 2>/dev/null | grep -qi 'mysql' \
  || fail "mysql secret type not registered"
echo "PASS"

echo "=== 4. mysql_* SETTINGS surface present ==="
out=$(printf "LOAD mysql_scanner;\nSELECT name FROM duckdb_settings() WHERE name IN ('mysql_debug_show_queries','mysql_experimental_filter_pushdown','mysql_tinyint1_as_boolean') ORDER BY 1;\n" \
  | "${DB[@]}" 2>/dev/null)
for s in mysql_debug_show_queries mysql_experimental_filter_pushdown mysql_tinyint1_as_boolean; do
  echo "$out" | grep -qi "$s" || fail "missing setting: $s"
done
echo "PASS"

echo "=== 5. CREATE SECRET (TYPE mysql) round-trips through the secret manager ==="
printf "LOAD mysql_scanner;\nCREATE SECRET my (TYPE mysql, HOST '127.0.0.1', PORT 3306, USER 'u', PASSWORD 'p', DATABASE 'd');\nSELECT name, type FROM duckdb_secrets() WHERE name='my';\n" \
  | "${DB[@]}" 2>/dev/null | grep -qiE 'my.*mysql' \
  || fail "CREATE SECRET (TYPE mysql) did not register"
echo "PASS"

echo "=== 6. ATTACH to an unreachable host -> reaches libmariadb connect (scanner wired to TCP) ==="
# The ATTACH binds, resolves the mysql catalog, and dispatches into MariaDB
# Connector/C's connect path -- i.e. it is fully wired, NOT "function/type not
# found". On the THREADLESS wasm core, Connector/C's connect spawns a worker
# thread before it ever reaches the socket, so the core aborts with
#   "thread constructor failed: Not supported"
# (identical to postgres_scanner's libpq on wasm). That abort -- not a clean
# libmariadb "Can't connect" -- is the expected outcome here until the core
# gains threads (or Connector/C is patched to connect single-threaded). If a
# real server is reachable AND the core is threaded, a genuine libmariadb
# connection error is also accepted.
out=$(printf "LOAD mysql_scanner;\nATTACH 'host=127.0.0.1 port=1 database=x user=y' AS my (TYPE mysql);\n" \
  | "${DB[@]}" 2>&1 | grep -ivE '\[wasi-fs\]' || true)
if echo "$out" | grep -qiE "thread constructor failed: Not supported"; then
  echo "PASS: reached Connector/C connect -> threadless-core wall (documented wasm limitation)"
elif echo "$out" | grep -qiE "Failed to connect to MySQL|Can't connect to server|connection refused"; then
  echo "PASS: reached libmariadb -> connection error (not 'function/type not found')"
else
  echo "UNEXPECTED (want the threadless-core wall or a libmariadb connection error):"; echo "$out" | tail -5
  fail "ATTACH did not reach the Connector/C connect path"
fi
