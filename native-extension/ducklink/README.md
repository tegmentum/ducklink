# ducklink (DuckDB community extension)

Run WebAssembly **component** extensions inside DuckDB.

A `duckdb:extension` component is built once and runs unmodified on every
platform DuckDB supports — no per-platform native extension builds. `ducklink`
embeds [wasmtime](https://wasmtime.dev), loads the component, runs its `load()`
to discover the functions it registers, and bridges them into DuckDB's catalog.

```sql
LOAD ducklink;
CALL ducklink_load('isin.wasm');
SELECT isin_is_valid('US0378331005');
```

## Two directions, one component

The same component artifact runs in both:

- **Direction 1** — the standalone `ducklink` host, which runs
  DuckDB-compiled-to-wasm and loads components alongside it.
- **Direction 2** — *this* extension, embedding wasmtime inside **native**
  DuckDB.

Both share the [`ducklink-runtime`](../../crates/ducklink-runtime) engine crate:
the `duckdb:extension` wasmtime bindings, the neutral `reg::*` registration
model, and the callback registry. A component therefore loads identically in
either direction.

## Layout

- `src/engine.rs` — the direction-agnostic engine glue: `Engine2::load` loads a
  component, runs its `load()`, and returns the `ScalarFunc`s it registered;
  `Engine2::dispatch_scalar` routes a DuckDB invocation back into the component
  through the shared callback registry. Depends only on `ducklink-runtime` +
  wasmtime, so it builds and is checked **without** the DuckDB toolchain.
- `src/lib.rs` — the `loadable` module (behind the `loadable` feature) holds the
  DuckDB C-API binding: the extension entry point and the per-function
  registration that maps a `ScalarFunc` onto a DuckDB scalar function.

## Build

The default build checks the engine glue against `ducklink-runtime`:

```
cargo check          # engine.rs, no DuckDB toolchain needed
```

The loadable artifact builds for the **native** host triple via the DuckDB Rust
C Extension API (`build: cargo`), separately from the wasm component workspace:

```
cargo build --features loadable --release
```

The community-extensions CI builds it with the `rust` and `python3` toolchains.
It is excluded from the `wasm_*` platforms (it embeds a JIT) and from the
static-musl / mingw triples.

## Status

The Direction-2 engine (`src/engine.rs`) is implemented and compiles against the
shared runtime. Remaining: the `loadable` C-API binding — register
`ducklink_load(path)` and bridge each `ScalarFunc` to a DuckDB scalar function
(per-row callback → `Engine2::dispatch_scalar`) — plus a native DuckDB build to
compile/test it end to end.
