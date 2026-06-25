---
id: building
title: Building & the lean core
sidebar_label: Building
---

# Building & the lean core

ducklink's components link against a statically built `libduckdb` compiled for
`wasm32-wasip2`. This guide covers prerequisites, cross-compiling that library,
building the components, and selecting what the core embeds.

## Prerequisites

1. **DuckDB source** at `DUCKDB_SOURCE_DIR` (a shallow clone is sufficient):
   ```bash
   git clone https://github.com/duckdb/duckdb.git ~/src/duckdb
   ```
2. **wasi-sdk** (tested with 33.0; exception handling requires ≥ 33) with
   `WASI_SDK_PREFIX` pointing at the installation root. A predownloaded copy lives
   under `external/wasi-sdk-33.0-<platform>`.
3. **Rust tooling** — `rustup target add wasm32-wasip2` and
   `cargo install cargo-component`.
4. **wit-bindgen tooling** (included automatically by `cargo-component`).

Network access is required only when fetching DuckDB or installing the toolchain.

## Building `libduckdb` for wasm

Use the helper script to cross-compile the library:

```bash
export DUCKDB_SOURCE_DIR=~/src/duckdb
export WASI_SDK_PREFIX="$(pwd)/external/wasi-sdk-33.0-arm64-macos"
export WASI_TARGET_TRIPLE=wasm32-wasip2
scripts/build-libduckdb-wasm.sh
```

The script places `libduckdb-wasi.a` under `artifacts/`. Then point the Rust build
at the headers and the archive:

```bash
export DUCKDB_INCLUDE_DIR="$DUCKDB_SOURCE_DIR/src/include"
export DUCKDB_STATIC_LIB="$(pwd)/artifacts/libduckdb-wasi.a"
```

:::warning Build for `wasm32-wasip2`, not `wasip1-threads`
DuckDB must target the same wasm triple the components link against. A
`wasm32-wasip1-threads` build traps on the first integer-literal parse in the
single-threaded runtime. See [the lean core page](../architecture/lean-core.md)
for the full explanation. Check `build/duckdb-wasi/compile_commands.json` for
`--target=` and `-pthread`; wipe `build/duckdb-wasi` and rebuild clean if the
target is wrong.
:::

## Selecting what the core embeds

The core is **fully lean by default** — only `core_functions` + `parquet`. Embed
DuckDB's in-tree C++ extensions with `EMBED_EXTENSIONS`:

```bash
# fully lean (default):
WASI_SDK_PREFIX=… DUCKDB_SOURCE_DIR=external/duckdb ./scripts/build-libduckdb-wasm.sh

# embed a chosen set (each also needs its prebuilt native deps present):
EMBED_EXTENSIONS="json,icu,httpfs,spatial" \
  WASI_SDK_PREFIX=… DUCKDB_SOURCE_DIR=external/duckdb ./scripts/build-libduckdb-wasm.sh
```

See [the lean core + de-embed program](../architecture/lean-core.md) for the full
selection model and the list of eligible extensions.

## Building the components

```bash
make            # builds both components (cargo component under the hood)

make core
make ducklink-cli

# browser-oriented core (needs a browser-compatible DuckDB static archive):
make core-browser BROWSER_TARGET=wasm32-unknown-unknown
```

The binaries land in `target/wasm32-wasip2/release/`:
`ducklink_core.wasm` and `ducklink_cli.wasm`.

## Running

```bash
# direct database access:
wasmtime component run target/wasm32-wasip2/release/ducklink_core.wasm --dir .

# compose CLI + core, then run a query:
cargo install wac-cli
wac plug target/wasm32-wasip2/release/ducklink_cli.wasm \
  --plug target/wasm32-wasip2/release/ducklink_core.wasm \
  -o artifacts/duckdb-cli.wasm
wasmtime run artifacts/duckdb-cli.wasm --dir . -- :memory: -c "select 42;"

# via the native host runner (composes core + cli + extension loader):
cargo run -p ducklink-host --bin ducklink -- -- duckdb-cli :memory: -c "select 42 as answer;"
```

## AOT precompilation

The core wasm's Cranelift compile (~7 s) otherwise happens on every cold start.
Precompiling skips it (load via deserialize, ~0.1 s). AOT artifacts are CPU- and
runtime-version specific — regenerate per target.

- **Standalone (native host):** `make precompile` → `ducklink precompile` produces
  `.cwasm` for the core + CLI components.
- **Browser / Node:** `jco transpile` the core to an AOT module.

## Testing & CI

```bash
make smoke-cli            # :memory: query via scripts/smoke-cli.sh
make smoke-cli-disk       # forces an on-disk temp database
make smoke-extension      # builds + loads the sample extension component
make ext-smoke-all        # smoke every extension
```

Continuous smoke coverage runs in `.github/workflows/smoke-tests.yml`; the same
workflow can run locally with [nektos/act](https://github.com/nektos/act) via
`make ci-local`.

## WIT packages

All WIT interfaces live under `wit/` at the repo root, vendoring the WASI Preview 2
packages plus the DuckDB-specific packages. The crate-local copies under
`crates/*/wit/` are generated from this canonical tree via `scripts/sync-*.sh` —
always edit `wit/` first, then re-run the sync scripts. External extensions can
depend on `wit/duckdb-extension/` to stay in sync with the host runtime.
