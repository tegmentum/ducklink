#!/usr/bin/env bash
#
# Larger-than-4-GiB spill demonstration for the TVM tier (opt-in: slow, needs
# ~5+ GiB free RAM for the host regions on top of the wasm 4 GiB instance).
#
# This is the headline capability: DuckDB runs in a wasm32 component capped at
# 4 GiB of linear memory, yet here it sorts a dataset whose spilled run set
# EXCEEDS 4 GiB. The spilled blocks live in host-owned TVM regions (the guest
# pools a fresh 1 GiB region whenever the active one fills), so the working set
# crosses the wasm 4 GiB ceiling -- which a single linear memory could never do.
#
# Asserts: (1) correct result, (2) cumulative bytes spilled into TVM > 4 GiB,
# (3) the guest opened multiple regions (>4 GiB peak => >=5 x 1 GiB regions),
# (4) no temp files.
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
TARGET_DIR="${TARGET_DIR:-$ROOT/target/wasm32-wasip2/release}"
HOST_BIN="${HOST_BIN:-$ROOT/target/release/ducklink}"
CORE_COMPONENT="${CORE_COMPONENT:-$TARGET_DIR/duckdb_core_component.wasm}"
CLI_COMPONENT="${CLI_COMPONENT:-$TARGET_DIR/duckdb_cli_component.wasm}"
MEMORY_LIMIT="${MEMORY_LIMIT:-512MB}"
ROWS="${ROWS:-650000000}"        # 650M int64 ~= 4.8 GiB sorted
MIN_SPILL_GIB="${MIN_SPILL_GIB:-4}"
MIN_REGIONS="${MIN_REGIONS:-5}"

for f in "$HOST_BIN" "$CORE_COMPONENT" "$CLI_COMPONENT"; do
  [[ -f "$f" ]] || { echo "missing: $f (build host + components first)" >&2; exit 1; }
done

TMP_DIR="$(mktemp -d)"; TRACE="$(mktemp)"
cleanup() { rm -rf "$TMP_DIR" "$TRACE"; }
trap cleanup EXIT

SQL="SET memory_limit='${MEMORY_LIMIT}';
SET threads=1;
SELECT count(*) AS n, min(i) AS lo, max(i) AS hi
FROM (SELECT i FROM range(${ROWS}) t(i) ORDER BY i DESC) sub;"

echo "==> sorting ${ROWS} rows under memory_limit=${MEMORY_LIMIT} (spills > ${MIN_SPILL_GIB} GiB to host TVM)"
OUT=$(printf '%s\n' "$SQL" | DUCKDB_TVM_DEBUG=1 "$HOST_BIN" \
  --core-component "$CORE_COMPONENT" --cli-component "$CLI_COMPONENT" \
  -- duckdb-cli :memory: 2>"$TRACE" | grep -avE "wasi-fs" || true)

fail() { echo "FAIL: $1" >&2; echo "$OUT" >&2; echo "--- trace tail ---" >&2; grep -aE '^\[tvm\]' "$TRACE" | tail -3 >&2; exit 1; }

# (1) correct result
echo "$OUT" | grep -q "| ${ROWS} " || fail "expected count(*) = ${ROWS}"
echo "$OUT" | grep -qE "\| 0 +\| $((ROWS-1)) " || fail "expected min=0 max=$((ROWS-1))"

# (2) > MIN_SPILL_GIB written into TVM
WRITTEN_MIB=$(grep -aoE "cumulative [0-9]+ MiB" "$TRACE" | grep -oE "[0-9]+" | sort -n | tail -1)
WRITTEN_MIB=${WRITTEN_MIB:-0}
[[ "$WRITTEN_MIB" -gt $((MIN_SPILL_GIB * 1024)) ]] || fail "only ${WRITTEN_MIB} MiB spilled; expected > ${MIN_SPILL_GIB} GiB"

# (3) multiple regions opened (peak live > 4 GiB => >= 5 x 1 GiB regions)
REGIONS=$(grep -acE "^\[tvm\] open region" "$TRACE")
[[ "$REGIONS" -ge "$MIN_REGIONS" ]] || fail "only ${REGIONS} TVM regions opened; expected >= ${MIN_REGIONS}"

# (4) no temp files
shopt -s nullglob
LEAKED=("$TMP_DIR"/*.tmp "$TMP_DIR"/*.block)
[[ ${#LEAKED[@]} -eq 0 ]] || fail "temp files written: ${LEAKED[*]}"

echo "PASS: ${ROWS}-row sort correct; $(awk "BEGIN{printf \"%.1f\", ${WRITTEN_MIB}/1024}") GiB spilled across ${REGIONS} host TVM regions (> 4 GiB, beyond the wasm ceiling); 0 temp files"
