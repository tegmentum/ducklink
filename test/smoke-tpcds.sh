#!/usr/bin/env bash
# Smoke test: the tpcds data generator + benchmark queries run inside the wasm
# DuckDB core.
#
# Requires the core component built against a tpcds-enabled libduckdb-wasi.a, i.e.
# the .a at ~/git/duckdb-wasm/build/duckdb-wasi/extension/tpcds/libtpcds_extension.a
# must be SELECTED for embedding:
#   EMBED_EXTENSIONS="...,tpcds" ./scripts/build-libduckdb-wasm.sh   (a core rebuild)
# Until then this smoke fails with: Catalog Error ... "dsdgen" ... exists in the
# tpcds extension.
#
# What it asserts (golden captured from native duckdb v1.5.2, sf=0.01):
#   * CALL dsdgen(sf=0.01) populates the TPC-DS tables with deterministic counts.
#       Scale-independent dimensions: ship_mode=20  income_band=20  reason=1
#                                     warehouse=1  web_site=1  call_center=1
#       Scaled (sf=0.01):  customer=1000  item=180  date_dim=73049
#   * tpcds_queries() exposes 99 queries.
#
# The wasm CLI's `-c` renders only the first statement, so the multi-statement
# script is piped via stdin (which renders each).
set -euo pipefail
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
cd "$ROOT"

HOST=./target/release/ducklink
[[ -x "$HOST" ]] || cargo build --release -p ducklink-host --bin ducklink

fail() { echo "FAIL: $*" >&2; exit 1; }

echo "=== 1. CALL dsdgen(sf=0.01) -> deterministic table row counts ==="
COUNTS=$(printf "%s\n" \
  ".mode csv" \
  "CALL dsdgen(sf=0.01);" \
  "SELECT 'ship_mode' t, count(*) c FROM ship_mode
     UNION ALL SELECT 'income_band', count(*) FROM income_band
     UNION ALL SELECT 'reason', count(*) FROM reason
     UNION ALL SELECT 'warehouse', count(*) FROM warehouse
     UNION ALL SELECT 'web_site', count(*) FROM web_site
     UNION ALL SELECT 'call_center', count(*) FROM call_center
     UNION ALL SELECT 'item', count(*) FROM item
     UNION ALL SELECT 'customer', count(*) FROM customer
     UNION ALL SELECT 'date_dim', count(*) FROM date_dim
     ORDER BY t;" \
  | "$HOST" -- duckdb-cli :memory: 2>&1)
echo "$COUNTS"
for pair in "ship_mode,20" "income_band,20" "reason,1" "warehouse,1" \
            "web_site,1" "call_center,1" "item,180" "customer,1000" \
            "date_dim,73049"; do
  echo "$COUNTS" | grep -qx "$pair" || fail "expected row count line '$pair'"
done

echo "=== 2. tpcds_queries(): 99 queries ==="
NQ=$(printf "%s\n" ".mode csv" "SELECT count(*) FROM tpcds_queries();" \
  | "$HOST" -- duckdb-cli :memory: 2>&1 | tail -1)
[[ "$NQ" == "99" ]] || fail "expected 99 tpcds queries, got '$NQ'"

echo "ALL TPC-DS SMOKE CHECKS PASSED"
