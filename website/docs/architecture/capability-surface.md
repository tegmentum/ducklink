---
id: capability-surface
title: The duckdb:extension capability surface
sidebar_label: Capability surface
---

# The `duckdb:extension` capability surface

Every ducklink extension is a Rust `wasm32-wasip2` component that implements the
`duckdb:extension` WIT world. The component registers itself imperatively in its
`load()` function; the host captures each registration and forwards it into the
DuckDB core. The set of registration shapes a component can use — the
**capability surface** — is the contract that decides whether *any* DuckDB
extension can ship as a portable, version-independent wasm component.

That surface is **complete**. The world exposes:

| Capability | What it registers | Dispatch |
|---|---|---|
| **scalar** | per-row functions (the default) | `call-scalar` callback per batch |
| **table** | table functions / replacement scans | table callback |
| **aggregate** | whole-batch aggregates (bloom/minhash/count-min/t-digest/…) | `call_aggregate` |
| **cast** | custom cast functions (no SQL form exists) | `call-cast` callback |
| **macro** | SQL macros (`CREATE MACRO`) | host runs DDL on a transient connection |
| **collation** | `ORDER BY x COLLATE …` collations | reuses the scalar callback |
| **pragma** | `PRAGMA name(...)` that can generate SQL | pragma callback + `spi.run-sql` |
| **catalog (ATTACH)** | `ATTACH … (TYPE …)` storage backends | storage-dispatch pull cursor |
| **files (FileSystem)** | virtual filesystems / replacement scans / copy handlers | files interface |
| **index (+ optimizer)** | `CREATE INDEX … USING HNSW`/R-tree + the planner rule that *chooses* it | index-dispatch |
| **custom type** | first-class logical types | `register-logical-type` + cast |

…all over a **rich logical type system** (booleans, the full integer/unsigned
family, float, text, blob, date/time/timestamp/timestamptz, interval, decimal,
uuid). See [the type-contract evolution](type-contract.md) for how that type
system is itself part of the contract.

## How registration is dispatched

A component never calls into DuckDB directly. The path is always:

1. The component calls a `runtime.register-*` function from `load()` (e.g.
   `register-scalar`, `register-aggregate`, `catalog.register-cast`).
2. The **host** (`ducklink-host`) captures it into a pending-registrations
   structure and drains it through the `extension-loader-hooks`.
3. The **core** (`ducklink-core`, Rust over DuckDB's C API) effects the real
   DuckDB registration — `duckdb_register_scalar_function`, a transient-connection
   `CREATE MACRO`, a `duckdb_create_cast_function`, etc.
4. At call time, DuckDB invokes the registered function; the core dispatches each
   value back to the component's callback (e.g. `call-scalar`) over the WIT
   boundary.

For capabilities that need a DuckDB **abstract C++ class** (catalog, files,
index), a small C++ translation unit is compiled into the wasm core (the **C++
shim pattern**): it subclasses the DuckDB class and forwards each method through
a Rust C-ABI bridge → a host WIT import → the component's dispatch export. This
is how `ATTACH (TYPE …)`, virtual filesystems, and custom indexes work without
the component ever touching DuckDB's internal C++ ABI.

## The two foundational techniques

Everything in the surface reuses two proven techniques:

- **The C++ shim pattern** — a C++ TU compiled into the wasm core with
  `sqlite_scanner`'s build flags (libc++, `-fwasm-exceptions`, legacy-eh=false),
  subclassing a DuckDB abstract class and forwarding to a Rust C-ABI bridge.
  Used by `wasm_storage.cpp`, `wasm_files.cpp`, and the index shim.
- **The C-lib-in-wasm build** — a native C/C++ library (SQLite, libpq, GEOS /
  PROJ / GDAL, openssl/curl) compiled to `wasm32-wasip2` and linked **inside** a
  component, behind the WIT boundary, so it never couples to DuckDB's C++ ABI.

## Capability coverage by deployment scenario

The same component runs across three [deployment scenarios](../guides/deployment.md).
Scenarios 2 (standalone wasm host) and 3 (browser) share the WebAssembly DuckDB
core, so they have identical, full coverage; scenario 1 (native ext) bridges each
capability onto native DuckDB's C API.

| Capability | 1. Native ext | 2. Wasm host | 3. Browser |
|---|:---:|:---:|:---:|
| Scalar functions | yes | yes | yes |
| Table functions | yes | yes | yes |
| Aggregate functions | yes | yes | yes |
| Cast / macro / replacement-scan | via core | yes | yes |

## Why this matters

When the full surface lands, the `duckdb:extension` world is **enough for any
DuckDB extension to ship as a portable, version-independent wasm component**. That
completeness — scalar, table, aggregate, cast, macro, collation, pragma +
`spi.run-sql`, catalog (ATTACH), files (virtual FileSystem), and index, over a
rich type system — is the deliverable. The capabilities and their verification
are covered in detail in [the capabilities reference](../capabilities/index.md).
