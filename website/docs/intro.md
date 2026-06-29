---
id: intro
title: Introduction
slug: /intro
sidebar_position: 1
---

# ducklink

**ducklink is DuckDB compiled to WebAssembly (`wasm32-wasip2`), plus a catalog of
~181 extension _components_ implementing the `duckdb:extension` WIT world.**

The core is **lean**: it statically embeds only DuckDB's base `core_functions`
and `parquet` (~44&nbsp;MB). Everything else — all official extensions and a
large set of community functionality — ships as loadable, composable WebAssembly
components rather than being baked into the binary. You add functionality with
`LOAD <name>`, not by recompiling the core.

## The lean-core + components model

A traditional DuckDB-on-wasm build statically links every extension you might
want into one large binary. ducklink inverts that:

- **The core does the minimum.** `core_functions` + `parquet` only. No optional
  extension is embedded by default. See [the lean core + de-embed
  program](architecture/lean-core.md).
- **Extensions are components.** Each is a Rust `wasm32-wasip2` component that
  implements the `duckdb:extension` WIT world and registers itself imperatively
  in `load()`. There is no per-platform native build, no statically-linked C++
  ABI to track — one portable `.wasm` per extension.
- **They compose.** A component can plug another component (via
  [`wac`](https://github.com/bytecodealliance/wac)) — e.g. `spatialproj`
  composes the GDAL component for `ST_Transform`. See
  [composition](architecture/composition.md).
- **They are version-independent.** The product contract is the **WIT interface +
  the wasm runtime**, not DuckDB's unstable C++ extension ABI. The DuckDB version
  built against is an internal detail behind that boundary. That contract is now
  [`duckdb:extension@4.0.0`](architecture/columnar-abi.md) — its hot dispatch path
  is **columnar** (typed columns, one bulk transfer per fixed-width column), which
  unlocks per-column SIMD kernels in the guest.

The whole `duckdb:extension` capability surface — scalar / table / aggregate /
cast / macro / collation / pragma / catalog (ATTACH) / files (FileSystem) /
index (+ optimizer) / custom type, over a rich logical type system — is
documented in [the capability surface](architecture/capability-surface.md).

## What ships

| Layer | What | Where |
|---|---|---|
| **Core** | `ducklink-core` — the DuckDB C API behind the `duckdb:component/database` world | `crates/ducklink-core` |
| **CLI** | `ducklink-cli` — a WASI-native shell mirroring the DuckDB CLI | `crates/ducklink-cli` |
| **Host** | `ducklink-host` (binary `ducklink`) — a Wasmtime runner that composes core + CLI + the component loader | `crates/ducklink-host` |
| **Extensions** | ~181 component extensions (the [catalog](catalog.md)) | `extensions/<name>-component` |

The same component, **built once**, runs unmodified across three
[deployment scenarios](guides/deployment.md): inside native DuckDB (via the
`ducklink` community extension that embeds Wasmtime), in the standalone wasm host,
and directly in a web browser.

## Quickstart

Run a query through the native host runner, which composes the CLI and core
components plus the extension loader:

```bash
cargo run -p ducklink-host --bin ducklink -- -- \
  duckdb-cli :memory: -c "select 42 as answer;"
```

Or compose the CLI + core into a single artifact and run it with
[`wasmtime`](https://wasmtime.dev/):

```bash
# Install the wac CLI once
cargo install wac-cli

# Compose the CLI + core component pair
wac plug target/wasm32-wasip2/release/ducklink_cli.wasm \
  --plug target/wasm32-wasip2/release/ducklink_core.wasm \
  -o artifacts/duckdb-cli.wasm

# Execute a query (grant directory access for any on-disk database file)
wasmtime run artifacts/duckdb-cli.wasm --dir . -- :memory: -c "select 42;"
```

Load an extension component at runtime:

```sql
LOAD baseN;
SELECT base32_encode('hello');
```

`LOAD <name>` pulls `artifacts/extensions/<name>.wasm` — no core recompile,
version-independent. That is the component model's whole point.

For building the core and the components from source, see
[building](guides/building.md).

## Where to go next

- **[Architecture](architecture/index.md)** — the WIT capability surface, the
  lean core + de-embed program, inter-component composition, and how the type
  contract evolves.
- **[Capabilities reference](capabilities/index.md)** — the storage pushdown /
  ATTACH catalog interface and the catalog/files/cast registrations.
- **[Extension catalog](catalog.md)** — the working components by category.
- **[Guides](guides/index.md)** — writing a component, building, embedding
  tracking, function prefixes, the HTTP server, the [JavaScript/TypeScript
  APIs](guides/javascript.md), [extension distribution over R2](guides/distribution.md),
  and deployment.
- **[Reference](reference/index.md)** — official + community extension status,
  the Iceberg surface, and [performance](reference/performance.md).

## Acknowledgments

This project owes a clear debt to [Simon Willison](https://simonwillison.net/)
and [`sqlite-utils`](https://sqlite-utils.datasette.io/). The extension catalog,
the scaffold → smoke → feedback tooling loop, and much of the CLI ergonomics
follow patterns Simon established with `sqlite-utils` and the wider Datasette
ecosystem. Many of the component extensions mirror utilities first popularized
there. Thank you.
