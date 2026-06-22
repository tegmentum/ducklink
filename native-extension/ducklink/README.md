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

## Build

This crate builds for the **native** host triple via the DuckDB Rust C Extension
API (`build: cargo`), separately from the wasm component workspace. The
community-extensions CI builds it with the `rust` and `python3` toolchains.

It is excluded from the `wasm_*` platforms (it embeds a JIT) and from the
static-musl / mingw triples.

## Status

Submission scaffold. The component-capture store-state is shared from
`ducklink-runtime`; the remaining work is lifting it out of the Direction-1 host
behind a sink trait so this crate supplies a C-API sink, then wiring the
per-row scalar bridge. See `src/lib.rs` and `src/component.rs`.
