#!/usr/bin/env bash
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
TARGET_DIR="${TARGET_DIR:-$ROOT/target/wasm32-wasip2/release}"
CLI_COMPONENT="${CLI_COMPONENT:-$TARGET_DIR/duckdb_cli_component.wasm}"
CORE_COMPONENT="${CORE_COMPONENT:-$TARGET_DIR/duckdb_core_component.wasm}"
OUTPUT_COMPONENT="${OUTPUT_COMPONENT:-$TARGET_DIR/duckdb_cli_standalone.wasm}"
SQL="${SQL:-select 1 as answer;}"
DB_PATH="${DB_PATH:-:memory:}"
EXTRA_WASMTIME_FLAGS="${EXTRA_WASMTIME_FLAGS:-}"
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

echo "Creating composed CLI component via wac plug..." >&2
wac plug "$CLI_COMPONENT" --plug "$CORE_COMPONENT" -o "$OUTPUT_COMPONENT"

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
cmd=(wasmtime run $EXTRA_WASMTIME_FLAGS "$OUTPUT_COMPONENT" --dir "$db_dir" --dir "$ROOT/artifacts" -- "$DB_PATH")
if [[ ${#load_flags[@]} -gt 0 ]]; then
  cmd+=("${load_flags[@]}")
fi
cmd+=(-c "$SQL")
"${cmd[@]}"
