#!/usr/bin/env bash
#
# Larger-than-memory spill test for the Tiered Virtual Memory (TVM) tier.
#
# DuckDB runs inside a wasm32 component capped at 4 GiB of linear memory. With a
# low `memory_limit`, a query that exceeds that budget must evict buffer-pool
# blocks. The patched buffer manager (standard_buffer_manager.cpp) routes those
# 256 KiB blocks through the `tvm:memory` imports into host-owned regions that
# live in the host's 64-bit address space -- so the spilled working set can grow
# past the wasm 4 GiB ceiling without ever writing a temp file.
#
# No temporary directory is configured: the patched BlockHandle::CanUnload makes
# temp blocks evictable whenever a TVM host is wired, so the spill goes straight
# to host regions with no on-disk temp file involved. (Without TVM this query
# errors with "Unused blocks cannot be offloaded to disk".)
#
# This sorts 20M rows under a 128 MB limit (~160 MB raw, forces a spill) and
# asserts: (1) correct result, (2) DuckDB pushed >memory_limit bytes into TVM,
# (3) no temp files were written.
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
TARGET_DIR="${TARGET_DIR:-$ROOT/target/wasm32-wasip2/release}"
HOST_BIN="${HOST_BIN:-$ROOT/target/release/duckdb-host}"
CORE_COMPONENT="${CORE_COMPONENT:-$TARGET_DIR/duckdb_core_component.wasm}"
CLI_COMPONENT="${CLI_COMPONENT:-$TARGET_DIR/duckdb_cli_component.wasm}"
MEMORY_LIMIT="${MEMORY_LIMIT:-128MB}"
ROWS="${ROWS:-20000000}"

for f in "$HOST_BIN" "$CORE_COMPONENT" "$CLI_COMPONENT"; do
  if [[ ! -f "$f" ]]; then
    echo "missing: $f (build the host with 'cargo build --release -p duckdb-component-host' and the components first)" >&2
    exit 1
  fi
done

TMP_DIR="$(mktemp -d)"
TRACE="$(mktemp)"
cleanup() { rm -rf "$TMP_DIR" "$TRACE"; }
trap cleanup EXIT

# Deliberately no `SET temp_directory`: TVM availability alone must make the
# blocks evictable (patched BlockHandle::CanUnload), so the spill goes to host
# regions with no temp file path involved.
SQL=$(cat <<SQL
SET memory_limit='${MEMORY_LIMIT}';
SET threads=1;
SELECT count(*) AS n, min(i) AS lo, max(i) AS hi
FROM (SELECT i FROM range(${ROWS}) t(i) ORDER BY i DESC) sub;
SQL
)

echo "==> sorting ${ROWS} rows under memory_limit=${MEMORY_LIMIT} (forces spill)"
OUT=$(printf '%s\n' "$SQL" | DUCKDB_TVM_DEBUG=1 "$HOST_BIN" \
  --dir "${TMP_DIR}::${TMP_DIR}" \
  --core-component "$CORE_COMPONENT" \
  --cli-component "$CLI_COMPONENT" \
  -- duckdb-cli :memory: 2>"$TRACE" | grep -avE "wasi-fs" || true)

fail() { echo "FAIL: $1" >&2; echo "--- query output ---" >&2; echo "$OUT" >&2; echo "--- tvm trace (tail) ---" >&2; tail -5 "$TRACE" >&2; exit 1; }

# (1) correct result
echo "$OUT" | grep -q "| ${ROWS} " || fail "expected count(*) = ${ROWS}"
echo "$OUT" | grep -qE "\| 0 +\| $((ROWS-1)) " || fail "expected min=0 max=$((ROWS-1))"

# (2) DuckDB pushed more than the memory_limit into host-owned TVM regions
grep -qE "^\[tvm\] open region" "$TRACE" || fail "no TVM region was opened (spill did not reach TVM)"
WRITTEN_MIB=$(grep -aoE "^\[tvm\] write [0-9]+ B \(cumulative [0-9]+ MiB" "$TRACE" | grep -oE "[0-9]+ MiB" | grep -oE "[0-9]+" | tail -1)
WRITTEN_MIB=${WRITTEN_MIB:-0}
[[ "$WRITTEN_MIB" -gt 128 ]] || fail "only ${WRITTEN_MIB} MiB reached TVM; expected > 128 (the memory_limit)"

# (3) no temp files leaked to disk
shopt -s nullglob
LEAKED=("$TMP_DIR"/*.tmp "$TMP_DIR"/*.block)
[[ ${#LEAKED[@]} -eq 0 ]] || fail "temp files written to disk: ${LEAKED[*]}"

echo "PASS: ${ROWS}-row sort correct; ${WRITTEN_MIB} MiB spilled through host TVM regions (> ${MEMORY_LIMIT} limit); 0 temp files"
