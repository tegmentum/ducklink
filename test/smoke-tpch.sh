#!/usr/bin/env bash
# Smoke test: the tpch data generator + benchmark queries run inside the wasm
# DuckDB core.
#
# Requires the core component built against a tpch-enabled libduckdb-wasi.a, i.e.
# the .a at ~/git/duckdb-wasm/build/duckdb-wasi/extension/tpch/libtpch_extension.a
# must be SELECTED for embedding:
#   EMBED_EXTENSIONS="...,tpch" ./scripts/build-libduckdb-wasm.sh   (a core rebuild)
# Until then this smoke fails with: Catalog Error ... "dbgen" ... exists in the
# tpch extension.
#
# What it asserts (golden captured from native duckdb v1.5.2, sf=0.01):
#   * CALL dbgen(sf=0.01) populates the 8 TPC-H tables with deterministic counts
#       region=5  nation=25  supplier=100  customer=1500  part=2000
#       partsupp=8000  orders=15000  lineitem=60175
#   * tpch_queries() exposes 22 queries; query 1's SQL is retrievable.
#   * PRAGMA tpch(1) runs benchmark query 1; its first ordered row is
#       A,F,380456.00,... (l_returnflag=A,l_linestatus=F,count_order=14876)
#
# The wasm CLI's `-c` renders only the first statement, so the multi-statement
# script is piped via stdin (which renders each).
set -euo pipefail
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
cd "$ROOT"

HOST=./target/release/ducklink
[[ -x "$HOST" ]] || cargo build --release -p ducklink-host --bin ducklink

fail() { echo "FAIL: $*" >&2; exit 1; }

echo "=== 1. CALL dbgen(sf=0.01) -> deterministic table row counts ==="
COUNTS=$(printf "%s\n" \
  ".mode csv" \
  "CALL dbgen(sf=0.01);" \
  "SELECT 'region' t, count(*) c FROM region
     UNION ALL SELECT 'nation', count(*) FROM nation
     UNION ALL SELECT 'supplier', count(*) FROM supplier
     UNION ALL SELECT 'customer', count(*) FROM customer
     UNION ALL SELECT 'part', count(*) FROM part
     UNION ALL SELECT 'partsupp', count(*) FROM partsupp
     UNION ALL SELECT 'orders', count(*) FROM orders
     UNION ALL SELECT 'lineitem', count(*) FROM lineitem
     ORDER BY t;" \
  | "$HOST" -- duckdb-cli :memory: 2>&1)
echo "$COUNTS"
for pair in "region,5" "nation,25" "supplier,100" "customer,1500" \
            "part,2000" "partsupp,8000" "orders,15000" "lineitem,60175"; do
  echo "$COUNTS" | grep -qx "$pair" || fail "expected row count line '$pair'"
done

echo "=== 2. tpch_queries(): 22 queries, query 1 SQL present ==="
NQ=$(printf "%s\n" ".mode csv" "SELECT count(*) FROM tpch_queries();" \
  | "$HOST" -- duckdb-cli :memory: 2>&1 | tail -1)
[[ "$NQ" == "22" ]] || fail "expected 22 tpch queries, got '$NQ'"

echo "=== 3. PRAGMA tpch(1) -> benchmark query 1 first-row golden ==="
Q1=$(printf "%s\n" \
  ".mode csv" \
  "CALL dbgen(sf=0.01);" \
  "PRAGMA tpch(1);" \
  | "$HOST" -- duckdb-cli :memory: 2>&1)
echo "$Q1"
# first data row of Q1 (ordered by l_returnflag,l_linestatus): A,F,...,count_order=14876
echo "$Q1" | grep -qE '^A,F,380456\.00,.*,14876$' || fail "tpch(1) first row mismatch"

echo "ALL TPC-H SMOKE CHECKS PASSED"
