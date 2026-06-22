#!/usr/bin/env bash
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
TARGET_DIR="${TARGET_DIR:-$ROOT/target/wasm32-wasip2/release}"
CLI_COMPONENT="${CLI_COMPONENT:-$TARGET_DIR/ducklink_cli.wasm}"
CORE_COMPONENT="${CORE_COMPONENT:-$TARGET_DIR/ducklink_core.wasm}"
STUB_COMPONENT="${STUB_COMPONENT:-$TARGET_DIR/ducklink_loader.wasm}"
CORE_LOADED_COMPONENT="${CORE_LOADED_COMPONENT:-$TARGET_DIR/ducklink_core_loaded.wasm}"
OUTPUT_COMPONENT="${OUTPUT_COMPONENT:-$TARGET_DIR/ducklink_cli_standalone.wasm}"
SQL="${SQL:-select 1 as answer;}"
DB_PATH="${DB_PATH:-:memory:}"
# The core component uses wasm C++ exceptions, so the standalone needs them on.
# `-C cache=y` enables wasmtime's on-disk compile cache so the (tens of MB)
# component is Cranelift-compiled once and deserialized on later runs instead of
# recompiled every invocation -- compilation otherwise dominates wall-clock.
EXTRA_WASMTIME_FLAGS="${EXTRA_WASMTIME_FLAGS:--W exceptions=y -C cache=y}"
ON_DISK_SMOKE="${ON_DISK_SMOKE:-0}"
TEMP_DB_DIR=""
EXTENSIONS="${EXTENSIONS:-}"

cleanup() {
  if [[ -n "$TEMP_DB_DIR" && -d "$TEMP_DB_DIR" ]]; then
    rm -rf "$TEMP_DB_DIR"
  fi
}
trap cleanup EXIT

if [[ ! -f "$CLI_COMPONENT" ]]; then
  echo "CLI component not found at $CLI_COMPONENT" >&2
  exit 1
fi

if [[ ! -f "$CORE_COMPONENT" ]]; then
  echo "Core component not found at $CORE_COMPONENT" >&2
  exit 1
fi

if [[ ! -f "$STUB_COMPONENT" ]]; then
  echo "Loader-stub component not found at $STUB_COMPONENT (run: make loader-stub)" >&2
  exit 1
fi

# The core declares three host imports for dynamic extension loading
# (host-extension-loader / extension-loader-hooks / callback-dispatch). The
# standalone has no native host to provide them, so first plug in the no-op
# loader stub, then plug the resulting core into the CLI.
echo "Composing standalone component via wac plug (core <- stub, cli <- core)..." >&2
wac plug "$CORE_COMPONENT" --plug "$STUB_COMPONENT" -o "$CORE_LOADED_COMPONENT"
wac plug "$CLI_COMPONENT" --plug "$CORE_LOADED_COMPONENT" -o "$OUTPUT_COMPONENT"

db_dir="."
if [[ "$ON_DISK_SMOKE" -ne 0 ]]; then
  TEMP_DB_DIR="$(mktemp -d "${TMPDIR:-/tmp}/duckdb-cli-smoke.XXXXXX")"
  DB_PATH="$TEMP_DB_DIR/smoke.duckdb"
  db_dir="$TEMP_DB_DIR"
else
  if [[ "$DB_PATH" != ":memory:" ]]; then
    db_dir="$(dirname "$DB_PATH")"
  fi
fi

load_flags=()
if [[ -n "$EXTENSIONS" ]]; then
  for extension in $EXTENSIONS; do
    load_flags+=("--load-extension" "$extension")
  done
fi

echo "Running wasmtime smoke test query..." >&2
set -x
# wasmtime options (incl. --dir preopens) MUST precede the module path; placed
# after it they are parsed as guest arguments and no directories are preopened.
cmd=(wasmtime run $EXTRA_WASMTIME_FLAGS --dir "$db_dir" --dir "$ROOT/artifacts" "$OUTPUT_COMPONENT" -- "$DB_PATH")
if [[ ${#load_flags[@]} -gt 0 ]]; then
  cmd+=("${load_flags[@]}")
fi
cmd+=(-c "$SQL")
"${cmd[@]}"
