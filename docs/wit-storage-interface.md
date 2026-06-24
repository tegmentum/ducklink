# WIT `storage` interface — pushdown scan + catalog ATTACH

Adds the surface a component needs to back an `ATTACH`-able catalog (DB-scanner
class) and the projection/filter **pushdown scan** primitive shared with heavy
bulk readers. The C/C++ library that actually reads the foreign store (here:
SQLite) is compiled to wasm32-wasip2 and lives **inside** the component, behind
this stable WIT boundary — so it never couples to DuckDB's internal C++ ABI.

**Compatibility constraint that shaped the design.** Each of the ~159 existing
components has its own *frozen* copy of the WIT and links against it. Adding a
case to the shared `capability` variant (or new types to `types`/`callback-
dispatch`) changes the structural type those components import/export, risking
instantiation breakage. So the storage surface is added as **new, separate
interfaces** plus a **separate world** — the shared `runtime` / `types` /
`callback-dispatch` interfaces are left byte-identical, and only a component
that opts into the new world is affected.

## Import side (component declares a backend in `load()`)

New interface `storage` (`wit/duckdb-extension/storage.wit`) — a free function,
not a capability-variant case:

```wit
register-storage: func(type-name: string,         // ATTACH TYPE, e.g. "sqlite"
                       callback-handle: u32,       // routed back to the component
                       options: option<extopts>) -> result<u32, duckerror>;
```

The host PROVIDES this import (captures the backend); that alone is what lets a
storage-world component instantiate in the production `ducklink` host.

## Pushdown types (new `storage` interface)

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

## Callback side (host pulls from the component)

Added to `callback-dispatch.wit`. The host drives a chunked **pull** cursor —
no streaming resource needed, so it stays plain-WIT and resumable:

```wit
// open a catalog from an ATTACH DSN (for sqlite-over-BLOB the dsn names a
// pre-staged blob; options carry key=value ATTACH params). -> catalog-handle.
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

1. **Provide + capture the `storage` import (done; in-sandbox).** The host
   implements `register-storage` to record the backend. This is the minimum that
   lets storage-world components instantiate in the production `ducklink` host,
   so the component's `sqlite_scan(blob, table)` table function (registered via
   the existing table path) is queryable and smoke-tested. Proves *SQLite-C in a
   wasm component serving DuckDB*.

2. **The pushdown scan path (done; proven by a test-local harness).** A
   wasmtime linker that provides `storage` and calls the component's
   `storage-dispatch` exports drives `attach-blob -> storage-attach ->
   storage-table-columns -> storage-scan-open(projection, filters, limit) ->
   storage-scan-next`. The `sqlite` component honors the projection + filters in
   the emitted SQL, so the test asserts a narrowed columnar result. Proves the
   *pushdown-scan WIT interface* end-to-end without DuckDB core changes.

3. **Literal `ATTACH ... (TYPE sqlite)` + engine-driven pushdown (follow-on).**
   A `WasmStorageExtension : StorageExtension` in the wasm-DuckDB-core whose
   `Catalog`/`SchemaCatalogEntry`/`TableCatalogEntry` forward to the same
   storage-dispatch callbacks, and which feeds DuckDB's optimizer projection/
   filter ids into `scan-request`. This is the only remaining piece that touches
   DuckDB C++ catalog classes (and a core rebuild); it adds NO new WIT.

## Proof target (increments 1 & 2)

SQLite compiled to wasm32-wasip2 *inside* `extensions/sqlite-component` (rusqlite
with the bundled SQLite C amalgamation, built by the wasi-sdk clang — confirmed
to compile). A DB is handed over as a BLOB and loaded via `sqlite3_deserialize`
(no shared FS). `sqlite_scan` is smoke-tested through the `ducklink` host;
projection + a pushed `WHERE` filter are proven through `storage-dispatch`.
