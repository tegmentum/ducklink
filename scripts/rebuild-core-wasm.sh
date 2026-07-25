#!/usr/bin/env bash
# Rebuild ducklink_core.wasm and copy it to every path the runtime + web front
# end look for.
#
# When the ducklink host bindgen (via `../duckdb-wasm/core/wit`) has moved past
# what the last built `target/wasm32-wasip2/release/ducklink_core.wasm` provides
# (Wasmtime linker error: "instance export ... has the wrong type / expected
# record of N fields, found M"), or when `web/public/ducklink_core.wasm` is
# stale and the callback-dispatch@<old> version doesn't line up with what
# `crates/ducklink-host` binds — you need to rebuild the core.
#
# Prerequisites (all provided by the sibling ducklink checkout by default):
#   WASI_SDK_PREFIX     wasi-sdk 33.0+ toolchain
#                       default: ../ducklink/external/wasi-sdk-33.0-<plat>
#   DUCKDB_STATIC_LIB   prebuilt libduckdb-wasi.a (~90 MB)
#                       default: ../duckdb-wasm/artifacts/libduckdb-wasi.a
#   DUCKDB_SOURCE_DIR   DuckDB checkout (headers)
#                       default: ../ducklink/external/duckdb
#   DUCKDB_BUILD_DIR    DuckDB wasm CMake build dir
#                       default: ../duckdb-wasm/build/duckdb-wasi
#   cargo-component     `cargo install cargo-component`
#   wasm-tools          `brew install wasm-tools` (verification only)
#
# See ../duckdb-wasm/scripts/setup-env.sh for how those defaults resolve; source
# it (or export the four env vars yourself) before running this script.

set -euo pipefail

DUCKLINK_ROOT="$(cd "$(dirname "$0")/.." && pwd)"
DUCKDB_WASM_DIR="${DUCKDB_WASM_DIR:-$DUCKLINK_ROOT/../duckdb-wasm}"
WASI_TARGET="${WASI_TARGET:-wasm32-wasip2}"

if [ ! -d "$DUCKDB_WASM_DIR" ]; then
  echo "error: DUCKDB_WASM_DIR ($DUCKDB_WASM_DIR) is not a directory" >&2
  exit 1
fi

# Populate env vars from setup-env.sh unless the caller already exported them.
# `source` is intentional so this script picks up the same defaults `make core`
# would use interactively. The initial `ln` warning inside setup-env.sh is
# harmless (symlink already exists) — do not let it abort us under `set -e`.
if [ -z "${WASI_SDK_PREFIX:-}" ] || [ -z "${DUCKDB_STATIC_LIB:-}" ]; then
  set +e
  # shellcheck disable=SC1091
  source "$DUCKDB_WASM_DIR/scripts/setup-env.sh"
  set -e
fi

: "${WASI_SDK_PREFIX:?set WASI_SDK_PREFIX to a wasi-sdk 33.0+ toolchain dir}"
: "${DUCKDB_STATIC_LIB:?set DUCKDB_STATIC_LIB to the prebuilt libduckdb-wasi.a}"
: "${DUCKDB_SOURCE_DIR:?set DUCKDB_SOURCE_DIR to the DuckDB source checkout}"
# libduckdb-sys reads duckdb.h from DUCKDB_INCLUDE_DIR; derive it here so the
# Makefile-facing var and the sys-crate-facing var stay consistent.
export DUCKDB_INCLUDE_DIR="${DUCKDB_INCLUDE_DIR:-$DUCKDB_SOURCE_DIR/src/include}"
export WASI_SDK_PREFIX DUCKDB_STATIC_LIB DUCKDB_SOURCE_DIR DUCKDB_BUILD_DIR

echo "-> sync duckdb-wasm/wit -> duckdb-wasm/core/wit"
"$DUCKDB_WASM_DIR/scripts/sync-core-wit.sh"

echo "-> cargo component build -p duckdb-component-core --target $WASI_TARGET --release --features wasi"
(
  cd "$DUCKDB_WASM_DIR"
  cargo component build -p duckdb-component-core --target "$WASI_TARGET" --release --features wasi
)

SRC="$DUCKDB_WASM_DIR/target/$WASI_TARGET/release/ducklink_core.wasm"
if [ ! -f "$SRC" ]; then
  echo "error: expected build artifact $SRC missing" >&2
  exit 1
fi

# Copy into every path the runtime + web front end look for. The host's
# `locate_component` (crates/ducklink-host/src/lib.rs) checks the wasm32-wasip2
# target dir; the browser bundle ships web/public/ducklink_core.wasm.
mkdir -p "$DUCKLINK_ROOT/target/$WASI_TARGET/release"
cp "$SRC" "$DUCKLINK_ROOT/target/$WASI_TARGET/release/ducklink_core.wasm"
mkdir -p "$DUCKLINK_ROOT/web/public"
cp "$SRC" "$DUCKLINK_ROOT/web/public/ducklink_core.wasm"

echo "-> imports in the new core:"
if command -v wasm-tools >/dev/null 2>&1; then
  wasm-tools component wit "$SRC" | grep -E "import duckdb:extension" || true
else
  echo "   (wasm-tools not on PATH — skipping import dump)"
fi

echo
echo "ok: ducklink_core.wasm rebuilt and staged"
echo "     $DUCKLINK_ROOT/target/$WASI_TARGET/release/ducklink_core.wasm"
echo "     $DUCKLINK_ROOT/web/public/ducklink_core.wasm"
