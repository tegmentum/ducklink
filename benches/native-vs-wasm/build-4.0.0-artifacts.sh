#!/usr/bin/env bash
# Reproduce the @4.0.0 artifact set for the native-vs-wasm matrix:
#   - NVW_ART:   the dynamic core (plain), the CLI, and the extension components
#   - NVW_EMBED: a core with the scalar extension(s) compiled IN (no WIT
#                dispatch) -- the embed framework, see duckdb-wasm branch
#                feat/embed-bench-4.0.0 (commit 339412f).
#
# The dynamic and embedded cores are built from the SAME duckdb-wasm tree so the
# dynamic-vs-embedded delta isolates the WIT dispatch boundary and nothing else.
# One combined embedded core (aba+fnv1a+siphash) is reused for all three
# embedded scalar cells: only the function under test is exercised per cell, and
# its dispatch path (the in-core native scalar, no WIT) is identical whether or
# not the other two scalars are also embedded -- so this is a faithful
# measurement, not a shortcut that changes the result.
#
# talib's sma() is an AGGREGATE; the embed framework is scalar-only, so there is
# no embedded talib core (that cell is honestly skipped).
#
# Binaries are gitignored (artifacts-*/, *.wasm); this script is the record.
set -euo pipefail

DUCKDB_WASM="${DUCKDB_WASM_DIR:-$HOME/git/duckdb-wasm}"      # branch feat/embed-bench-4.0.0
DUCKLINK="${DUCKLINK_DIR:-$HOME/git/ducklink}"               # @4.0.0 (CLI + extensions)
HERE="$(cd "$(dirname "$0")" && pwd)"
ART="$HERE/artifacts-4.0.0"
EMB="$ART/embedded"

export WASI_SDK_PREFIX="${WASI_SDK_PREFIX:-$DUCKLINK/external/wasi-sdk-33.0-arm64-macos}"
export DUCKDB_STATIC_LIB="${DUCKDB_STATIC_LIB:-$DUCKDB_WASM/artifacts/libduckdb-wasi.a}"
export DUCKDB_SOURCE_DIR="${DUCKDB_SOURCE_DIR:-$DUCKLINK/external/duckdb}"
export DUCKDB_BUILD_DIR="${DUCKDB_BUILD_DIR:-$DUCKDB_WASM/build/duckdb-wasi}"

mkdir -p "$ART/extensions" "$EMB/aba" "$EMB/checksums" "$EMB/siphash"
CORE_OUT="$DUCKDB_WASM/target/wasm32-wasip2/release/ducklink_core.wasm"
CLI_OUT="$DUCKLINK/target/wasm32-wasip2/release/ducklink_cli.wasm"

cd "$DUCKDB_WASM"
echo "[1/3] embedded core (embed-aba,embed-fnv1a,embed-siphash)"
cargo component build -p duckdb-component-core --target wasm32-wasip2 --release \
  --features wasi,embed-aba,embed-fnv1a,embed-siphash
cp "$CORE_OUT" "$EMB/aba/ducklink_core.wasm"
cp "$CORE_OUT" "$EMB/checksums/ducklink_core.wasm"
cp "$CORE_OUT" "$EMB/siphash/ducklink_core.wasm"

echo "[2/3] dynamic core (plain wasi)"
cargo component build -p duckdb-component-core --target wasm32-wasip2 --release --features wasi
cp "$CORE_OUT" "$ART/ducklink_core.wasm"

echo "[3/3] CLI component"
cd "$DUCKLINK"
./scripts/sync-cli-wit.sh 2>/dev/null || true
cargo component build -p ducklink-cli --target wasm32-wasip2 --release
cp "$CLI_OUT" "$ART/ducklink_cli.wasm"

for e in aba checksums siphash talib; do
  cp "$DUCKLINK/artifacts/extensions/$e.wasm" "$ART/extensions/$e.wasm"
done

# Native runner @4.0.0 (bundled DuckDB) -- speaks the current WIT contract:
#   SUBMODULE_DIR="$DUCKLINK/native-extension/ducklink" bash native-runner/build.sh
echo "done. NVW_ART=$ART  NVW_EMBED=$EMB"
