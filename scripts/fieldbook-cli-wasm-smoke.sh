#!/usr/bin/env bash
# Smoke-test the standalone `fieldbook-cli.wasm` composed by `make fieldbook-cli`.
#
# Verifies that the wac-plugged CLI + core + fieldbook-loader compose loads
# under wasmtime, opens a scratch DuckDB, executes SQL, and reports success
# on `LOAD fieldbook;` (via the loader stub's `request_load("fieldbook") ->
# true`). The engine-level fieldbook scalars (`fieldbook_create` /
# `fieldbook_add_entry` / ...) are NOT exercised here — fieldbook.wasm targets
# `duckdb:extension@5.0.0` while the current core is @4.0.0 (see
# docs/fieldbook-wasm-phase0-findings.md §2.3), so the scalar surface is a
# follow-up gated on the core v4->v5 rebuild.
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
ARTIFACT="${ARTIFACT:-$ROOT/artifacts/cli/fieldbook-cli.wasm}"

command -v wasmtime >/dev/null 2>&1 \
  || { echo "error: wasmtime not on PATH — install wasmtime first." >&2; exit 1; }
[[ -f "$ARTIFACT" ]] \
  || { echo "error: $ARTIFACT missing — run 'make fieldbook-cli' first." >&2; exit 1; }

# The core component uses wasm C++ exceptions and its cold Cranelift compile
# takes ~7s; mirror the flags smoke-cli.sh uses so the second run is warm.
WASMTIME_FLAGS="${WASMTIME_FLAGS:--W exceptions=y -C cache=y}"

WORKDIR="$(mktemp -d "${TMPDIR:-/tmp}/fieldbook-cli-smoke.XXXXXX")"
trap 'rm -rf "$WORKDIR"' EXIT
DB="$WORKDIR/demo.duckdb"

echo "--- fieldbook-cli.wasm smoke ---"
echo "artifact: $ARTIFACT"
echo "wasmtime: $(wasmtime --version)"
echo "workdir : $WORKDIR"

echo
echo "-- test 1: -c 'SELECT 1' round-trips a scalar through the composed CLI"
# wasmtime `--dir` MUST precede the module path; flags after the path are
# forwarded to the guest as argv.
wasmtime run $WASMTIME_FLAGS --dir "$WORKDIR" "$ARTIFACT" -- "$DB" \
  -c "SELECT 1 AS answer;" \
  | grep -v '^\[wasi-fs\]' \
  | tee "$WORKDIR/select-1.out"
grep -q '| 1 ' "$WORKDIR/select-1.out" \
  || { echo "FAIL: SELECT 1 did not return the expected row." >&2; exit 1; }

echo
echo "-- test 2: 'LOAD fieldbook;' reports success via the fieldbook-loader stub"
wasmtime run $WASMTIME_FLAGS --dir "$WORKDIR" "$ARTIFACT" -- "$DB" \
  -c "LOAD fieldbook;" \
  | grep -v '^\[wasi-fs\]' \
  | tee "$WORKDIR/load-fieldbook.out"
grep -q 'Success' "$WORKDIR/load-fieldbook.out" \
  || { echo "FAIL: LOAD fieldbook did not report success." >&2; exit 1; }

echo
echo "PASS: fieldbook-cli.wasm loads, executes SQL, and honours LOAD fieldbook."
echo "NOTE: engine scalars (fieldbook_create/...) and .fb/.entry/.run dot commands"
echo "      are deferred; see docs/fieldbook-wasm-phase0-findings.md §2.3, §4.2."
