---
id: storage-pushdown
title: Storage pushdown + catalog ATTACH
sidebar_label: Storage pushdown
---

# WIT `storage` interface — pushdown scan + catalog ATTACH

The `storage` interface adds the surface a component needs to back an
`ATTACH`-able catalog (the DB-scanner class) and the projection/filter **pushdown
scan** primitive shared with heavy bulk readers. The C/C++ library that actually
reads the foreign store (here: SQLite) is compiled to `wasm32-wasip2` and lives
**inside** the component, behind this stable WIT boundary — so it never couples to
DuckDB's internal C++ ABI.

:::info Why a separate world
Each existing component links against its own *frozen* copy of the WIT. Adding a
case to the shared `capability` variant (or new types to `types` /
`callback-dispatch`) changes the structural type those components import/export,
risking instantiation breakage (see [the type contract](../architecture/type-contract.md)).
So the storage surface is added as **new, separate interfaces** plus a **separate
world** — the shared interfaces stay byte-identical, and only a component that
opts into the new world is affected.
:::

## Import side — the component declares a backend in `load()`

A new `storage` interface (`wit/duckdb-extension/storage.wit`) provides a free
function (not a capability-variant case):

```wit
register-storage: func(type-name: string,         // ATTACH TYPE, e.g. "sqlite"
                       callback-handle: u32,       // routed back to the component
                       options: option<extopts>) -> result<u32, duckerror>;
```

The host **provides** this import (capturing the backend); that alone is what lets
a storage-world component instantiate in the production `ducklink` host.

## Pushdown types

```wit
interface storage {
  use types.{duckvalue, columndef};

  enum compare-op { eq, ne, lt, le, gt, ge, is-null, is-not-null }

  // One AND-ed predicate pushed into a scan. `column` indexes the table's
  // FULL column list (not the projection). Pushdown is best-effort: the engine
  // re-applies every filter, so a backend may ignore any it cannot evaluate.
  record scan-filter {
    column: u32,
    op: compare-op,
    value: duckvalue,        // ignored for is-null / is-not-null
  }

  record scan-request {
    table: string,
    projection: list<u32>,   // column indices to emit, in order; empty = all
    filters: list<scan-filter>,
    limit: option<u64>,      // best-effort row cap
  }
}
```

## Callback side — the host pulls from the component

Added to `callback-dispatch.wit`. The host drives a chunked **pull** cursor — no
streaming resource needed, so it stays plain-WIT and resumable:

```wit
// open a catalog from an ATTACH DSN. -> catalog-handle.
storage-attach:       func(handle: u32, dsn: string,
                           options: list<tuple<string, string>>)
                           -> result<u32, duckerror>;

storage-list-tables:  func(handle: u32, catalog: u32)
                           -> result<list<string>, duckerror>;

storage-table-columns:func(handle: u32, catalog: u32, table: string)
                           -> result<list<columndef>, duckerror>;

// open a scan cursor for one (catalog, table) with pushdown. -> scan-handle.
storage-scan-open:    func(handle: u32, catalog: u32, request: scan-request)
                           -> result<u32, duckerror>;

// pull up to max-rows; an empty resultset signals EOF. Columns are emitted in
// projection order (or natural order if projection was empty).
storage-scan-next:    func(handle: u32, scan: u32, max-rows: u32)
                           -> result<resultset, duckerror>;

storage-scan-close:   func(handle: u32, scan: u32) -> result<bool, duckerror>;
storage-detach:       func(handle: u32, catalog: u32) -> result<bool, duckerror>;
```

## How the host maps this to DuckDB — three increments

1. **Provide + capture the `storage` import (done).** The host implements
   `register-storage` to record the backend — the minimum that lets storage-world
   components instantiate, so the component's `sqlite_scan(blob, table)` table
   function is queryable and smoke-tested. Proves *SQLite-C in a wasm component
   serving DuckDB*.
2. **The pushdown scan path (done).** A wasmtime linker provides `storage` and
   drives `attach-blob → storage-attach → storage-table-columns →
   storage-scan-open(projection, filters, limit) → storage-scan-next`. The
   `sqlite` component honors the projection + filters in the emitted SQL, so the
   test asserts a narrowed columnar result. Proves the pushdown-scan WIT interface
   end-to-end with **no DuckDB core changes**.
3. **Literal `ATTACH … (TYPE sqlite)` + engine-driven pushdown (done).** A
   `WasmStorageExtension : StorageExtension` in the wasm core whose
   `Catalog`/`SchemaCatalogEntry`/`TableCatalogEntry` forward to the same
   storage-dispatch callbacks, feeding DuckDB's optimizer projection/filter ids
   into `scan-request`. This is the only piece that touches DuckDB C++ catalog
   classes (and a core rebuild); it adds **no new WIT**.

## Increment 3 — the C++ `StorageExtension` shim (done, verified)

```sql
LOAD sqlitewasm;
ATTACH '/tmp/m2.sqlite' AS db (TYPE sqlitewasm);
SELECT a FROM db.t WHERE a > 1;   -- returns 2
```

The trace shows `dispatch_storage_scan_open … projection=[0] filters=[(col 0
CompareOp::Gt 1)]` — engine-driven projection + filter pushdown into the wasm
component's in-wasm SQLite, behind the WIT boundary.

The wasm core is Rust over DuckDB's C API; literal `ATTACH (TYPE x)` needs
`StorageExtension::Register(DBConfig&, …)` + `Catalog` subclasses (C++-only), and
engine-driven pushdown needs `TableFunctionInitInput::{column_ids, filters}` (not
exposed by the C table-function API). So a C++ translation unit is added to the
core build, modeled on the embedded `sqlite_scanner`'s own
`sqlite_catalog`/`sqlite_schema_entry`/`sqlite_table_entry` — "sqlite_scanner, but
the storage backend is a wasm component over WIT."

**Division of labor.** The C++ TU does the DuckDB catalog plumbing + pushdown
extraction; the Rust core does the WIT marshalling (reusing existing code) via a
C-ABI bridge:

```c
uint32_t wasm_storage_attach(const char* dsn, /* options */);        // -> catalog
size_t   wasm_storage_list_tables(uint32_t cat, /* out names */);
size_t   wasm_storage_table_columns(uint32_t cat, const char* table, /* out (name,typecode) */);
uint32_t wasm_storage_scan_open(uint32_t cat, const char* table,
            const uint32_t* proj, size_t nproj,
            const WasmScanFilter* filters, size_t nfilt, int64_t limit); // -> scan
bool     wasm_storage_scan_fill(uint32_t scan, duckdb_data_chunk out);  // false = EOF
void     wasm_storage_scan_close(uint32_t scan);
void     wasm_storage_detach(uint32_t cat);
```

:::tip Key finding
`create_transaction_manager` must be non-null or DuckDB silently falls back to
native file storage. The ABI matched `sqlite_scanner`'s flags (libc++,
`-fwasm-exceptions`, legacy-eh=false).
:::

## Proof target

SQLite compiled to `wasm32-wasip2` **inside** `extensions/sqlite-component`
(rusqlite with the bundled SQLite C amalgamation, built by the wasi-sdk clang). A
DB is handed over as a BLOB and loaded via `sqlite3_deserialize` (no shared FS).
`sqlite_scan` is smoke-tested through the `ducklink` host; projection + a pushed
`WHERE` filter are proven through `storage-dispatch`. This proves a **heavy C
library can be a durable WIT component** that DuckDB drives over a stable
interface.
