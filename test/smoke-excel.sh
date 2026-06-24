#!/usr/bin/env bash
# Smoke test: the excel extension reads/writes .xlsx and formats numbers, all
# through the wasm DuckDB core. Surface exercised (matches native duckdb 1.5.2
# `INSTALL excel; LOAD excel;`):
#   1. COPY ... TO 'f.xlsx' WITH (FORMAT xlsx)   -- write a table to .xlsx
#   2. read_xlsx('f.xlsx')                        -- read it back (round-trip)
#   3. text(...) / excel_text(...)                -- Excel number-format scalars
#
# REQUIRES: a libduckdb-wasi.a with excel embedded. excel is NOT in the current
# core (its libexcel_extension.a exists under
# ~/git/duckdb-wasm/build/duckdb-wasi/extension/excel/ but wasn't selected for
# embedding). To enable: add `excel` to EMBED_EXTENSIONS and rebuild the core via
# scripts/build-libduckdb-wasm.sh. cmake/wasm-extension-config.cmake already wires
# the build (minizip-ng + expat-wasm deps). Until then this smoke FAILS at the
# first read_xlsx with "Catalog Error: ... read_xlsx does not exist".
#
# Round-trip note: .xlsx stores every number as a double, so an INTEGER `id`
# column reads back as DOUBLE (1 -> 1.0). The golden output below reflects that.
#
# The xlsx is written into a writable temp dir preopened as guest /work, so no
# binary fixture is committed -- the write half of the round-trip is exercised
# live. The host preopens with full file perms (DirPerms::all/FilePerms::all).
#
# NB: the wasm CLI's `-c` renders only the first statement, so the multi-step
# round-trip is piped via stdin (which renders each), like smoke-aws.sh.
set -euo pipefail
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
cd "$ROOT"

HOST=./target/release/ducklink
[[ -x "$HOST" ]] || cargo build --release -p ducklink-host --bin ducklink

WORK="$(mktemp -d)"
trap 'rm -rf "$WORK"' EXIT

echo "=== excel round-trip (COPY TO xlsx -> read_xlsx) + number-format scalars ==="
printf '%s\n' \
  ".mode csv" \
  "CREATE TABLE t AS SELECT * FROM (VALUES (1,'alice',1.5),(2,'bob',2.25),(3,'carol',3.0)) AS v(id,name,amount);" \
  "COPY t TO '/work/rt.xlsx' WITH (FORMAT xlsx, HEADER true);" \
  "SELECT * FROM read_xlsx('/work/rt.xlsx') ORDER BY id;" \
  "SELECT text(1234.567, '#,##0.00') AS fmt_number;" \
  "SELECT excel_text(0.25, '0.0%') AS fmt_percent;" \
  | "$HOST" --dir "$WORK::/work" -- duckdb-cli :memory:

cat <<'GOLDEN'

=== expected (golden) ===
id,name,amount
1.0,alice,1.5
2.0,bob,2.25
3.0,carol,3.0
fmt_number
"1,234.57"
fmt_percent
25.0%
GOLDEN
