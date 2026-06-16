#!/usr/bin/env bash
set -euo pipefail

REPO_ROOT="$(cd "$(dirname "$0")/.." && pwd)"
WIT_ROOT="$REPO_ROOT/wit"
CLI_DIR="$REPO_ROOT/crates/duckdb-cli-component/wit"

rm -rf "$CLI_DIR"
mkdir -p "$CLI_DIR/deps/duckdb/deps"

cp "$WIT_ROOT/standalone/duckdb-cli.wit" "$CLI_DIR/duckdb-cli.wit"
cp "$WIT_ROOT/core/duckdb-core.wit" "$CLI_DIR/deps/duckdb/component.wit"
cp "$WIT_ROOT/core/deps.toml" "$CLI_DIR/deps/duckdb/deps.toml"

rm -rf "$CLI_DIR/deps/duckdb-extension"
cp -R "$WIT_ROOT/duckdb-extension" "$CLI_DIR/deps/duckdb-extension"
rm -rf "$CLI_DIR/deps/duckdb/deps/duckdb-extension"
cp -R "$WIT_ROOT/duckdb-extension" "$CLI_DIR/deps/duckdb/deps/duckdb-extension"

for dep in "$WIT_ROOT"/deps/*; do
  name="$(basename "$dep")"
  rm -rf "$CLI_DIR/deps/$name"
  cp -R "$dep" "$CLI_DIR/deps/$name"
done
