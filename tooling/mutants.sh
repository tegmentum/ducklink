#!/usr/bin/env bash
# Run cargo-mutants on the native FFI bridge (reg_duckdb.rs) -- the most
# safety-critical code in the tree: the scalar/table/aggregate dispatch +
# NULL/validity handling + the catch_unwind guard wrappers that stop a wasm panic
# from aborting the host. A surviving mutant = a behaviour the test suite does
# not pin down.
#
# Baseline tests: the in-crate `--lib` unit tests (guard, type_code, env-spec,
# and the end-to-end sample-component dispatch). They need the `bundled` feature
# (an in-process DuckDB compiled from source) and a fresh sample artifact at
# artifacts/extensions/sample_extension.wasm.
#
# Usage:
#   tooling/mutants.sh                 # mutate reg_duckdb.rs
#   tooling/mutants.sh --list          # just list the mutants
set -euo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
BRIDGE_DIR="$REPO_ROOT/native-extension/ducklink"

# Refresh the sample component artifact the baseline tests load, so the WIT world
# matches the local runtime (a stale artifact fails the linker, reddening the
# baseline).
if command -v cargo-component >/dev/null 2>&1; then
  ( cd "$REPO_ROOT/extensions/sample-extension-component" \
      && cargo component build --target wasm32-wasip2 --release >/dev/null 2>&1 ) || true
  SRC="$REPO_ROOT/target/wasm32-wasip1/release/sample_extension_component.wasm"
  [ -f "$SRC" ] && cp "$SRC" "$REPO_ROOT/artifacts/extensions/sample_extension.wasm"
fi

cd "$BRIDGE_DIR"

# Mutate only the bridge file. The `--features`/`--no-default-features` flags
# apply to BOTH the baseline build and every per-mutant build+test (the default
# `loadable` feature can't link in a test build, so the bundled in-process DuckDB
# is required). The trailing `-- --lib` restricts the test run to the in-crate
# unit tests (guard, type_code, env-spec, sample-component dispatch).
#
# `--in-place` is REQUIRED here: this crate depends on `ducklink-runtime` via a
# git rev that the ducklink workspace root's `.cargo/config.toml` path-overrides
# to the LOCAL `crates/ducklink-runtime` (which has the current WIT). cargo-mutants'
# default copy-to-tmpdir loses that parent-dir override (the git rev's older WIT
# is used instead -> the baseline build fails). Mutating in place keeps the
# override; cargo-mutants restores each mutated file after testing it.
exec cargo mutants \
  --file src/reg_duckdb.rs \
  --in-place \
  --no-default-features \
  --features bundled \
  --timeout 180 \
  -- --lib \
  "$@"
