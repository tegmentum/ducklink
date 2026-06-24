#!/usr/bin/env bash
# Smoke test: the postgres_scanner extension (libpq compiled inline for wasm,
# TCP over httpfs's wasip2 socket graft, TLS via openssl-wasm) loads, registers
# its catalog (postgres_scan / postgres_query / postgres_attach + ATTACH ...
# TYPE postgres), exposes the postgres SECRET type and pg_* SETTINGS, and that an
# ATTACH to an unreachable host fails with a real libpq CONNECTION error -- i.e.
# the scanner is fully wired through to the TCP stack, not merely "function not
# found".
#
# This is an OFFLINE smoke (no live PostgreSQL server required), mirroring
# smoke-aws.sh: everything asserted here is deterministic without a server. To
# exercise a real round-trip, start a throwaway server first, e.g.:
#   docker run --rm -e POSTGRES_PASSWORD=pw -p 5432:5432 postgres:16
# then ATTACH 'host=host.docker.internal port=5432 dbname=postgres user=postgres
# password=pw' AS pg (TYPE postgres); -- but that is NOT required to pass.
#
# REQUIREMENT TO RUN IN THE WASM CORE: rebuild the core with postgres_scanner (and
# httpfs, for the socket graft) embedded --
#   EMBED_EXTENSIONS="httpfs,postgres_scanner" ./scripts/build-libduckdb-wasm.sh
# (its libpostgres_scanner_extension.a is built but not yet in the linked core).
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

echo "=== 1. postgres_scanner loads + is reported loaded ==="
printf "LOAD postgres_scanner;\nSELECT extension_name, loaded FROM duckdb_extensions() WHERE extension_name='postgres_scanner';\n" \
  | "${DB[@]}" 2>/dev/null | grep -qiE 'postgres_scanner.*true' \
  || fail "postgres_scanner did not report loaded=true"
echo "PASS"

echo "=== 2. catalog: postgres_scan / postgres_query / postgres_attach registered ==="
out=$(printf "LOAD postgres_scanner;\nSELECT function_name FROM duckdb_functions() WHERE function_name IN ('postgres_scan','postgres_query','postgres_attach') ORDER BY 1;\n" \
  | "${DB[@]}" 2>/dev/null)
for fn in postgres_attach postgres_query postgres_scan; do
  echo "$out" | grep -qi "$fn" || fail "missing function: $fn"
done
echo "PASS"

echo "=== 3. postgres SECRET type registered ==="
printf "LOAD postgres_scanner;\nSELECT type FROM duckdb_secret_types() WHERE type='postgres';\n" \
  | "${DB[@]}" 2>/dev/null | grep -qi 'postgres' \
  || fail "postgres secret type not registered"
echo "PASS"

echo "=== 4. pg_* SETTINGS surface present ==="
out=$(printf "LOAD postgres_scanner;\nSELECT name FROM duckdb_settings() WHERE name IN ('pg_connection_cache','pg_pages_per_task','pg_debug_show_queries') ORDER BY 1;\n" \
  | "${DB[@]}" 2>/dev/null)
for s in pg_connection_cache pg_debug_show_queries pg_pages_per_task; do
  echo "$out" | grep -qi "$s" || fail "missing setting: $s"
done
echo "PASS"

echo "=== 5. CREATE SECRET (TYPE postgres) round-trips through the secret manager ==="
printf "LOAD postgres_scanner;\nCREATE SECRET pg (TYPE postgres, HOST '127.0.0.1', PORT 5432, USER 'u', PASSWORD 'p', DATABASE 'd');\nSELECT name, type FROM duckdb_secrets() WHERE name='pg';\n" \
  | "${DB[@]}" 2>/dev/null | grep -qiE 'pg.*postgres' \
  || fail "CREATE SECRET (TYPE postgres) did not register"
echo "PASS"

echo "=== 6. ATTACH to an unreachable host -> real libpq connection error (scanner wired to TCP) ==="
out=$(printf "LOAD postgres_scanner;\nATTACH 'host=127.0.0.1 port=1 dbname=x user=y connect_timeout=2' AS pg (TYPE postgres);\n" \
  | "${DB[@]}" 2>&1 | grep -ivE '\[wasi-fs\]' || true)
if echo "$out" | grep -qiE 'Unable to connect to Postgres|connection .*failed|Connection refused'; then
  echo "PASS: reached libpq -> connection error (not 'function/type not found')"
else
  echo "UNEXPECTED (want a libpq connection error):"; echo "$out" | tail -5
  fail "ATTACH did not produce a connection error"
fi
