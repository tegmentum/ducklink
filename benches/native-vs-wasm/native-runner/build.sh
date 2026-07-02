#!/usr/bin/env bash
# Build the NATIVE runner.
#
# The runner is compiled INSIDE the native `ducklink` extension submodule so it
# resolves `ducklink-runtime` through the monorepo's `.cargo/config.toml`
# paths-override (to the in-tree `crates/ducklink-runtime`), exactly as the
# extension's own end-to-end bench does -- this guarantees the native dispatch
# glue speaks the same WIT ABI as the prebuilt components it loads.
#
# This copies the committed runner source (src/main.rs) into the submodule as a
# transient `src/bin/` target and builds it with the same feature set the
# submodule's own end-to-end bench uses (`--no-default-features --features
# bundled`). The submodule checkout is otherwise untouched (the gitlink is not
# moved; the copied file is build input only).
set -euo pipefail

HERE="$(cd "$(dirname "$0")" && pwd)"
SUBMODULE_DIR="${SUBMODULE_DIR:-$HERE/../../../native-extension/ducklink}"

if [ ! -f "$SUBMODULE_DIR/Cargo.toml" ]; then
  echo "error: submodule not found at $SUBMODULE_DIR" >&2
  echo "  run: git submodule update --init native-extension/ducklink" >&2
  exit 1
fi

mkdir -p "$SUBMODULE_DIR/src/bin"
cp "$HERE/src/main.rs" "$SUBMODULE_DIR/src/bin/nvw_native_runner.rs"

# Work around a stale call site in the submodule HEAD (24c9268). The in-tree
# `crates/ducklink-runtime` makes `CallbackEntry.extension` an `Arc<str>` (a
# per-row dispatch perf change) that indexes `HashMap<String,_>` via
# `Borrow<str>`; the submodule's engine.rs still passes `&entry.extension` (an
# `&Arc<str>`), which no longer satisfies the bound. Deref to `&str`. Idempotent;
# works whether the field is `Arc<str>` or `String`. Applied to the build
# checkout only -- the committed submodule (gitlink) is not modified.
sed -i.bak 's/\.get_mut(&entry\.extension)/.get_mut(\&*entry.extension)/g' \
  "$SUBMODULE_DIR/src/engine.rs" && rm -f "$SUBMODULE_DIR/src/engine.rs.bak"

echo "building nvw_native_runner in $SUBMODULE_DIR (bundled DuckDB) ..."
( cd "$SUBMODULE_DIR" && cargo build --release --no-default-features --features bundled --bin nvw_native_runner )

echo "built: $SUBMODULE_DIR/target/release/nvw_native_runner"
