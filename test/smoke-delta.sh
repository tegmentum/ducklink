#!/usr/bin/env bash
# Smoke test: read a local Delta Lake table through the wasm DuckDB core.
# Requires the core component built against a delta-enabled libduckdb-wasi.a
# (scripts/build-libduckdb-wasm.sh with duckdb_extension_load(delta)) + the
# ducklink runner. Fixture: test/fixtures/delta_people (built by deltalake).
set -euo pipefail
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
cd "$ROOT"

FIXTURE="$ROOT/test/fixtures/delta_people"
[[ -d "$FIXTURE/_delta_log" ]] || { echo "missing fixture: $FIXTURE" >&2; exit 1; }

HOST=./target/release/ducklink
[[ -x "$HOST" ]] || cargo build --release -p ducklink-host --bin ducklink

# Preopen the fixtures dir as guest /fixtures; run delta_scan over it.
"$HOST" --dir "$ROOT/test/fixtures::/fixtures" -- \
  duckdb-cli :memory: -c \
  "SELECT * FROM delta_scan('/fixtures/delta_people') ORDER BY id;"
