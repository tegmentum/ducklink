#!/usr/bin/env bash
# Regenerate the DuckLink core JS bindings via `wit-js-bindgen --role runtime-guest`.
#
# Emits `web/bindings-runtime-guest/libduckdb.runtime-guest.mjs` +
# `libduckdb.runtime-guest.d.mts`. The `.mjs` inlines the wasm-cm tagged
# byte-frame codec (25 tags) and exports two entry points consumers wire up
# at boot time:
#
#   - `registerHostProviders(driver, impls)` — one provider handle per WIT
#     import interface, each backed by a byte-frame router that decodes the
#     args frame, dispatches to `impls[iface][key]`, and encodes the return.
#   - `bindRuntimeGuest(driver, instanceHandle)` — pulls every exported
#     interface off the instance and wraps its funcs as ergonomic JS
#     callables (byte-frame in / byte-frame out).
#
# ## CRITICAL: bindings are derived from the WASM BINARY, not from the WIT tree
#
# The DuckLink WIT sources under `wit/core/wit` have drifted ahead of the built
# `ducklink_core.wasm` on prior landings (F1's finding in elena-wasm's runtime
# investigation). Deriving bindings from the WIT tree emits import names the
# wasm doesn't have, and instantiation fails with "missing import"; deriving
# from the wasm keeps import names in lockstep with whatever the wasm actually
# links against, at the cost of losing any WIT-only decoration wit-js-bindgen
# would pull from the sources (interface docstrings, etc.).
#
# The default source is `web/public/ducklink_core.wasm` — the copy the demo
# fetches at runtime. Override with `DUCKLINK_CORE_WASM=/absolute/path.wasm`.
#
# Requires the `wit-js-bindgen` binary from the sibling checkout at
# `~/git/wit-js-bindgen` (build once with `cargo build --release`). Override
# with `WIT_JS_BINDGEN=/other/path/wit-js-bindgen`.

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"

WIT_JS_BINDGEN="${WIT_JS_BINDGEN:-$HOME/git/wit-js-bindgen/target/release/wit-js-bindgen}"
DUCKLINK_CORE_WASM="${DUCKLINK_CORE_WASM:-$SCRIPT_DIR/public/ducklink_core.wasm}"
OUT_DIR="$SCRIPT_DIR/bindings-runtime-guest"

if [[ ! -x "$WIT_JS_BINDGEN" ]]; then
    echo "error: wit-js-bindgen not found or not executable at $WIT_JS_BINDGEN" >&2
    echo "" >&2
    echo "  Clone tegmentum/wit-js-bindgen and build once:" >&2
    echo "    git clone https://github.com/tegmentum/wit-js-bindgen ~/git/wit-js-bindgen" >&2
    echo "    (cd ~/git/wit-js-bindgen && cargo build --release)" >&2
    echo "" >&2
    echo "  Or set WIT_JS_BINDGEN to an existing build:" >&2
    echo "    WIT_JS_BINDGEN=/path/to/wit-js-bindgen bash web/generate.sh" >&2
    exit 1
fi

if [[ ! -f "$DUCKLINK_CORE_WASM" ]]; then
    echo "error: DUCKLINK_CORE_WASM=$DUCKLINK_CORE_WASM does not exist." >&2
    echo "" >&2
    echo "  The default path is $SCRIPT_DIR/public/ducklink_core.wasm — the copy" >&2
    echo "  `npm run copy-wasm` (or `predev`) stages next to index.html. Build" >&2
    echo "  the ducklink core (see repo README) and copy it into place, or" >&2
    echo "  override with DUCKLINK_CORE_WASM=/absolute/path/to/ducklink_core.wasm." >&2
    exit 1
fi

rm -rf "$OUT_DIR"
mkdir -p "$OUT_DIR"

"$WIT_JS_BINDGEN" "$DUCKLINK_CORE_WASM" \
    --world root --role runtime-guest \
    --out "$OUT_DIR"

# Rename the emitter's world-named output (`root.runtime-guest.*`) to the
# `libduckdb`-prefixed shape `run-core.mjs` imports. Keeps the consumer's
# import path stable across wasm rebuilds that don't change the emitted
# world name.
mv "$OUT_DIR/root.runtime-guest.mjs" "$OUT_DIR/libduckdb.runtime-guest.mjs"
cp "$OUT_DIR/root.runtime-guest.d.ts" "$OUT_DIR/libduckdb.runtime-guest.d.mts"
mv "$OUT_DIR/root.runtime-guest.d.ts" "$OUT_DIR/libduckdb.runtime-guest.d.ts"

echo "generated (byte-frame runtime-guest bindings, from wasm):"
ls -1 "$OUT_DIR" | sed 's/^/  /'
