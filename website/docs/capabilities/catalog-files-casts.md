---
id: catalog-files-casts
title: Catalog, files & cast registrations
sidebar_label: Catalog, files & casts
---

# Catalog, files & cast registrations

Beyond scalar / table / aggregate functions, a component can register **macros**,
**logical types**, **casts**, and **replacement scans** through the `catalog` and
`files` interfaces of the `duckdb:extension` world. The capability model is
7-variant (scalar, table, aggregate, plus `catalog` and `file-format`), and these
interfaces are part of the extension world, satisfied by the host.

## What the migration added

- `wit/duckdb-extension/types.wit` — `enum capabilitykind` includes `catalog` and
  `file-format` (7 variants), synced to all crates and the sample.
- `wit/duckdb-extension/catalog.wit` — `interface catalog` (`register-logical-type`,
  `register-cast`, `register-macro`).
- `wit/duckdb-extension/files.wit` — `interface files` (`register-replacement-scan`,
  `register-copy-handler`).
- The extension world imports `catalog`/`files`; the host implements
  `extension_catalog::Host` + `extension_files::Host` and adds them to the
  extension linker so components that import them instantiate.

## What maps onto a real DuckDB C-API path

| Registration | C API path | Status |
|---|---|---|
| macro | none — `CREATE MACRO` SQL only | **working** |
| replacement scan | `duckdb_add_replacement_scan` | **working** |
| logical type | none — `CREATE TYPE` SQL alias | **working** |
| cast | `duckdb_create_cast_function` + cast callback | **working** |
| copy handler | none (no DuckDB C-API copy-function registration) | **not feasible** — `register-copy-handler` returns a clear error |

### Macros

`sample-extension-component` calls `catalog.register-macro`; the host captures it
(`pending_macros`), drains it through `extension-loader-hooks`, and the core builds
`CREATE OR REPLACE MACRO …` and runs it on a **transient connection** to each
active database (never the LOAD-busy connection). Verified:
`select sample_add_two(40)` → 42.

This required enabling **wasm C++ exceptions** (so a caught `throw` unwinds to an
error instead of `std::terminate → abort`). wasi-sdk-33 ships an exception-handling
`eh` multilib; the toolchain adds `-fwasm-exceptions -mllvm
-wasm-use-legacy-eh=false` (the standardized encoding wasmtime's production
`exceptions` feature accepts). A side benefit: every DuckDB SQL error became a
recoverable error instead of a fatal module abort (`SELECT * FROM nonexistent` now
returns a Catalog Error).

### Replacement scans

`sample-extension-component` registers a `sample_read_path(VARCHAR)` table
function and calls `files.register-replacement-scan(...)`. The host resolves the
table-function handle to its name and forwards a `replacement-scan-registration`;
the core installs one global `duckdb_add_replacement_scan` callback per database
that rewrites `FROM 'file.ext'` to the registered table function. Verified:
`select * from 'hello.sample'` → row `hello.sample`.

### Casts

The only catalog/files type needing a real transformation **callback** (no SQL
form exists). It reuses the scalar callback-dispatch machinery:

- WIT — a `cast-callback` resource, `call-cast`, and a `callback` param on
  `catalog.register-cast`.
- host — `CallbackKind::Cast`, `HostCastCallback`, `dispatch_cast`.
- core — `register_pending_cast` resolves the source/target type names to
  `duckdb_logical_type` via `SELECT CAST(NULL AS <name>)`, creates a
  `duckdb_create_cast_function`, and the callback reads the input vector,
  dispatches each value to the guest, and writes the output vector.

Verified: a `VARCHAR → sample_id` cast parsing `"id-7"` into `7` —
`cast('id-7' as sample_id)` → 7 (the built-in VARCHAR→integer cast fails on
`"id-7"`, so a 7 proves the custom callback ran).

### Logical types

`catalog.register-logical-type({name, physical})` is forwarded as a
`logical-type-registration`; the core runs `CREATE TYPE <name> AS <physical>` on a
transient connection (a named SQL type alias). Verified: the sample registers
`sample_id AS INTEGER`, and `select 7::sample_id` → 7.

## Build notes

Two build issues surfaced and were fixed alongside this work:

- **The `HTTPUtil` vtable blocker.** The WASI toolchain sets `DUCKDB_SKIP_HTTP`,
  excluding the TU that emits `HTTPUtil`'s vtable, but core DuckDB still
  constructed an `HTTPUtil`. Fixed by guarding the construction with
  `#ifndef DUCKDB_SKIP_HTTP` so `http_util` stays null when HTTP is skipped.
- **Core logging.** The core is a reactor component whose host doesn't wire
  `wasi:cli/stderr`, so std `eprintln!` aborted DuckDB mid-load; core logging now
  goes through a non-panicking `clog!` macro.
