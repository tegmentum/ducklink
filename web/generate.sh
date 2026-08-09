#!/usr/bin/env bash
# Regenerate the DuckLink core JS bindings from the canonical WIT tree
# using `wit-js-bindgen` — elena-wasm's WIT -> JS/TS binding generator.
#
# Emits `web/bindings/libduckdb.mjs` + `web/bindings/libduckdb.d.mts`.
# `libduckdb.mjs` carries the full canonical-ABI marshaling glue plus an
# `instantiate(module, imports, extraImports?)` factory; `libduckdb.d.mts`
# carries the WIT-derived TypeScript types (kebab-name -> camelCase, WIT
# variants -> discriminated unions).
#
# Consumer role only -- the browser is *calling* the wasm-hosted DuckDB,
# never satisfying its interface. `--auto-alias-wasi` widens the emitted
# stub table with `wasi:*@0.2.<N>` fallbacks for every `0..D` below the
# declared `0.2.<D>` so a component that imports the same interface at
# multiple minor tags links against one set of impls.
#
# Requires the `wit-js-bindgen` binary from the sibling checkout at
# `~/git/wit-js-bindgen` (build once with `cargo build --release`).
# Override the binary path with `WIT_JS_BINDGEN=/other/path/wit-js-bindgen`
# and the source WIT tree with `DUCKLINK_WIT_DIR=/other/wit/core/wit`.

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"

WIT_JS_BINDGEN="${WIT_JS_BINDGEN:-$HOME/git/wit-js-bindgen/target/release/wit-js-bindgen}"
WIT_DIR="${DUCKLINK_WIT_DIR:-$REPO_ROOT/wit/core/wit}"
OUT_DIR="$SCRIPT_DIR/bindings"

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

if [[ ! -d "$WIT_DIR" ]]; then
    echo "error: WIT tree not found at $WIT_DIR" >&2
    echo "" >&2
    echo "  The default path is a symlink to the sibling duckdb-wasm checkout" >&2
    echo "  at \`~/git/duckdb-wasm/core/wit\`. Check that the sibling clone" >&2
    echo "  exists, or override with DUCKLINK_WIT_DIR=/absolute/path/to/wit." >&2
    exit 1
fi

rm -rf "$OUT_DIR"
mkdir -p "$OUT_DIR"

"$WIT_JS_BINDGEN" "$WIT_DIR" \
    --world libduckdb \
    --role consumer \
    --auto-alias-wasi \
    --out "$OUT_DIR"

# Match elena-wasm's generated-bindings shape: rename .d.ts -> .d.mts so
# TypeScript's `bundler`/`nodenext` resolution pairs the declarations
# with their .mjs siblings.
for f in "$OUT_DIR"/*.d.ts; do
    [[ -e "$f" ]] || continue
    mv "$f" "${f%.d.ts}.d.mts"
done

echo "generated:"
ls -1 "$OUT_DIR" | sed 's/^/  /'
