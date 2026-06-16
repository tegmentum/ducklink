#!/usr/bin/env bash
set -euo pipefail

REPO_ROOT="$(cd "$(dirname "$0")/.." && pwd)"
WIT_ROOT="$REPO_ROOT/wit"
CORE_DIR="$REPO_ROOT/crates/duckdb-core-component/wit"

rm -rf "$CORE_DIR"
mkdir -p "$CORE_DIR/deps"

cp "$WIT_ROOT/core/duckdb-core.wit" "$CORE_DIR/duckdb-core.wit"
cp "$WIT_ROOT/core/deps.toml" "$CORE_DIR/deps.toml"
rm -rf "$CORE_DIR/deps/duckdb-extension"
cp -R "$WIT_ROOT/duckdb-extension" "$CORE_DIR/deps/duckdb-extension"

for dep in "$WIT_ROOT"/deps/*; do
  name="$(basename "$dep")"
  rm -rf "$CORE_DIR/deps/$name"
  cp -R "$dep" "$CORE_DIR/deps/$name"
done
