---
id: lean-core
title: The lean core + de-embed program
sidebar_label: Lean core + de-embed
---

# The lean core + de-embed program

ducklink's core is **fully lean by default**: the wasm archive contains only
DuckDB's base `core_functions` + `parquet` (~44&nbsp;MB, a ~64% reduction versus
embedding the official extension set). Everything optional is opt-in.

## How extension selection works

DuckDB's own first-party extensions (`json`, `icu`, `httpfs`, …) are **C++**, not
Rust components. They cannot be runtime wasm plugins on `wasm32-wasip2` — there is
no dynamic linking, and DuckDB's extension API is deep C++ internals, not a narrow
SPI. So they are statically compiled into `artifacts/libduckdb-wasi.a` at the
**libduckdb cmake build**, and the generated builtin-extension loader
hard-references each one. The selection therefore happens at the libduckdb build,
not later.

The selector is **`EMBED_EXTENSIONS`** — a comma-separated list read by both
`cmake/wasm-extension-config.cmake` and `scripts/build-libduckdb-wasm.sh`. Default
empty means fully lean. Naming an extension gates, in lockstep, (a) its
`duckdb_extension_load`, (b) its source staging + patches, and (c) merging its
native-dep archives — so an unselected extension adds nothing.

```bash
# fully lean (default) — only core_functions + parquet:
WASI_SDK_PREFIX=… DUCKDB_SOURCE_DIR=external/duckdb ./scripts/build-libduckdb-wasm.sh

# embed a chosen set (each also needs its prebuilt native deps present):
EMBED_EXTENSIONS="json,icu,httpfs,spatial" \
  WASI_SDK_PREFIX=… DUCKDB_SOURCE_DIR=external/duckdb ./scripts/build-libduckdb-wasm.sh
```

:::note
`WASM_EXTENSIONS` is **not** the selector — it only flips DuckDB's internal
`WASM_ENABLED` flag. `EMBED_EXTENSIONS` selects what is statically linked.
:::

`crates/libduckdb-sys/build.rs` auto-discovers every `extension/<name>/lib*.a` the
build produced, so the core links exactly the embedded set with no Rust-side edit.

:::warning Build for `wasm32-wasip2`, not `wasip1-threads`
DuckDB must be compiled for the same wasm target the component links against.
`wasm32-wasip1-threads` is a `-pthread` build where `errno`/`__thread` are
thread-local; in the single-threaded component runtime that TLS isn't
established, so the first integer-literal parse faults in `core_yylex` /
`process_integer_literal` — an obscure symptom that looks like "json is broken."
`scripts/build-libduckdb-wasm.sh` defaults `WASI_TARGET_TRIPLE=wasm32-wasip2`;
don't let it fall back to the toolchain default.
:::

## The de-embed program

The lean core deliberately **removes** functionality that DuckDB normally
provides via embedded extensions, then re-delivers it as components. This is the
de-embed program. Its purpose is to prove the [capability
surface](capability-surface.md) is complete: every official extension whose
surface fits the existing WIT capabilities (scalar / table / aggregate / cast /
macro / pragma / catalog / files) can leave the core.

De-embed components deliberately register **official names** so they transparently
replace the removed embedded versions:

- `jsonfns` registers `json_valid`/`json_extract`/… (it autoloads).
- `inetfns` registers `host`/`family`/`netmask`/…
- `spatialfns` registers `ST_*`.
- `parquetfns` and others fill the remaining gaps.

Because two such components could register the same official name, this motivates
[function prefixes](../guides/prefixes.md) (SPARQL-style namespacing) so operators
can see collisions and pin which implementation wins the bare name.

Only DuckDB's **in-tree** extensions are eligible to embed at all
(`autocomplete`, `core_functions`, `icu`, `json`, `parquet`, `tpch`, `tpcds`).
Out-of-tree official extensions (`httpfs`, `spatial`, `fts`, `excel`, `inet`,
`vss`, `sqlite_scanner`, …) live in separate repos; their wasm feasibility and
status are tracked in [the official extensions reference](../reference/official-extensions.md).

## What remains in the core

After the de-embed pass, the only meaningful core gap versus a full build is
`json` — re-delivered by `jsonfns`. The lean default is `core_functions` +
`parquet`; eight officials that were historically embedded are now componentized.
The set a given build actually embeds is tracked by [embedding
tracking](../guides/embedding-tracking.md).
