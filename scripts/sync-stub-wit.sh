#!/usr/bin/env bash
set -euo pipefail

REPO_ROOT="$(cd "$(dirname "$0")/.." && pwd)"
WIT_ROOT="$REPO_ROOT/wit"
STUB_DIR="$REPO_ROOT/crates/ducklink-loader/wit"

rm -rf "$STUB_DIR"
mkdir -p "$STUB_DIR/deps/duckdb/deps"

cp "$WIT_ROOT/standalone/loader-stub.wit" "$STUB_DIR/loader-stub.wit"
cp "$WIT_ROOT/core/duckdb-core.wit" "$STUB_DIR/deps/duckdb/component.wit"
cp "$WIT_ROOT/core/deps.toml" "$STUB_DIR/deps/duckdb/deps.toml"

rm -rf "$STUB_DIR/deps/duckdb-extension"
cp -R "$WIT_ROOT/duckdb-extension" "$STUB_DIR/deps/duckdb-extension"
rm -rf "$STUB_DIR/deps/duckdb/deps/duckdb-extension"
cp -R "$WIT_ROOT/duckdb-extension" "$STUB_DIR/deps/duckdb/deps/duckdb-extension"

for dep in "$WIT_ROOT"/deps/*; do
  name="$(basename "$dep")"
  rm -rf "$STUB_DIR/deps/$name"
  cp -R "$dep" "$STUB_DIR/deps/$name"
done
