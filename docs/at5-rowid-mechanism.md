# At-@5 rowid mechanism for storage-dispatch extensions

Phase 2c implemented Amendment A5's UPDATE/DELETE pre-scan on the host side:
before dispatching `storage_write_update` / `storage_write_delete`, the host
resolves the WHERE predicate to concrete rowids via a synthetic SELECT
against the read path, then dispatches the write with the collected
`rowids: list<s64>`. Phase 2d owed the *extension side* of the contract:
how the extension advertises "this is the rowid column" so the host can
extract it from the scan output.

This memo records what the @5 WIT actually specifies, the options available,
and the choice made for `sqlitewasm`, `mysqlwasm`, `postgreswasm`, and
`unityscan`.

## 1. What the @5 WIT specifies

Reading `crates/ducklink-runtime/wit/deps/duckdb-extension/` end-to-end:

- **`storage.wit` `scan-request`** (@5):
  ```wit
  record scan-request {
    table: string,
    projection: list<u32>,   // column indices to emit, in order; empty = all
    filters: list<scan-filter>,
    limit: option<u64>,
  }
  ```
  Compared to @4 there is NO `wants-rowid` field. The @4 in-tree
  `WasmStorageExtension` used to detect the engine's virtual rowid column and
  set that flag; the C-API table-function pushdown surface at @5 has no such
  concept, so the field was dropped. The extension no longer receives an
  explicit "give me rowids" hint from the host per scan.

- **`storage-dispatch.wit` `storage-table-columns`**:
  ```wit
  storage-table-columns: func(handle: u32, catalog: u32, table: string)
                              -> result<list<columndef>, duckerror>;
  ```
  Returns an ordered `list<columndef>`. There is no separate call that
  advertises a rowid column, no distinguished flag on `columndef`, no
  variant marker. Whatever the extension puts here is what the host
  materializes as the table's shape (via
  `HostState::intercept_attach` -> `register_table_function` -> `CREATE
  VIEW <alias>.main.<table>`).

- **`storage-write-dispatch.wit` `update-rows`/`delete-rows`**:
  ```wit
  delete-rows: func(handle, txn, table, rowids: list<s64>)
  update-rows: func(handle, txn, table, rowids: list<s64>, rows: list<list<duckvalue>>)
  ```
  Both consume rowids as opaque `s64`s. Where those come from is not
  specified by the WIT — that is the read-path <-> write-path bridge the
  host and extension have to agree on out-of-band.

- **Host side (`crates/ducklink-host/src/lib.rs`)**:
  - `at5_locate_rowid_column` (line ~5063) walks the columns returned by
    `storage-table-columns` and finds the first entry whose `name` is
    equal (case-insensitive) to `"rowid"`.
  - `at5_prescan_rows` (line ~4986) runs `SELECT * FROM {alias}.main.{table}
    [WHERE pred]` through the materialized view, then reads column
    `rowid_idx` from each row and expects an integer arm of `duckvalue`
    there (via `at5_duckvalue_to_i64`, accepts any signed/unsigned int).

**Conclusion:** the @5 mechanism is by-convention, not by-WIT. The
contract that satisfies the host's already-landed Phase 2c code is:
**the extension MUST include a column whose case-insensitive name is
"rowid" in `storage-table-columns`, and MUST emit an integer rowid in
that column's position on every `storage-scan-next` batch row**.

## 2. Options considered

- **(A) Synthetic `rowid` column in `table-columns`.** Case-insensitive
  name match, integer arm in the scan output, position exposed to the
  host through the columndef list. Zero additional WIT surface. The
  host code (`at5_locate_rowid_column`, `at5_prescan_rows`) is already
  wired to this.

- **(B) A separate dispatch call.** Something like
  `storage-fetch-rowids(catalog, table, filters) -> list<s64>` on
  `storage-dispatch`. Cleaner separation (rowids don't pollute the
  user's `SELECT *`), but requires: WIT surface change (breaking
  additive), extra host-side dispatch trampoline, and an extra
  round-trip. Also the host already synthesizes the pre-scan through
  the read path, so this call would duplicate that logic.

- **(C) A `columndef` flag.** Add `is-rowid: bool` (or a `role` enum)
  to `columndef`. Cleaner than (A) name-based; still requires a WIT
  surface change that would ripple through the entire columnar ABI.
  Deferred: the @5 columnar hot path (`callback-dispatch`) is unrelated,
  and evolving `columndef` mid-major is not worth the churn.

**Choice: Option A**, because it is what the already-landed host code
consumes. Amendment A5 explicitly says (line 249 of the ADR):

> Their `storage-dispatch.table-columns(table)` must include a
> synthetic `rowid` column (or an extension-defined stable row-key
> column named `rowid`). `sqlitewasm`, `mysqlwasm`, `postgreswasm` all
> expose one already (SQLite has native rowid; MySQL InnoDB has
> `_rowid`; Postgres via ctid or a synthetic ordinal).

## 3. Trade-off: `rowid` leaks into `SELECT *`

Because the extension advertises `rowid` as a regular column entry in
`storage-table-columns`, the synthetic table function the host creates
via `intercept_attach` (`__<alias>_<table>()`) reports the same shape.
The `CREATE VIEW <alias>.main.<table> AS SELECT * FROM __<alias>_<table>()`
materialisation therefore exposes `rowid` as a real column. Every
`SELECT * FROM <alias>.<table>` returns it, and every user-facing
column enumeration (DESCRIBE, information_schema) sees it.

This is a UX cost, but it is a **direct consequence** of the @5 read
path routing writes through a pre-scan against the same view the user
queries. Hiding it would require the intercept_attach layer to
register the table function with a *filtered* columndef list (rowid
omitted) while still using the *full* list for the write pre-scan —
extra state the host doesn't currently keep. Reasonable Phase 5
follow-up; documented v1 wart. Same trade-off native DuckDB accepts
for its own hidden `rowid` — users just don't `SELECT *` when they
mean `SELECT everything except rowid`.

## 4. Per-extension mapping

For each of the four in-tree storage extensions we own:

### sqlitewasm-component

- **Native concept:** SQLite's `ROWID` pseudo-column. Every ordinary
  SQLite table has one, INTEGER, stable within a transaction, monotonic
  by default. `SELECT rowid FROM t` is a plain SQL query.
- **table-columns:** prepend `rowid: Int64` to whatever `PRAGMA
  table_info` reported (index 0).
- **scan:** the SQL builder in `run_scan` selects `rowid` alongside the
  projected columns and emits it in the `rowid` slot when the
  projection asks for it (or when the projection is empty -> "all
  columns").
- **write path:** SQLite `DELETE FROM t WHERE rowid IN (...)` and
  `UPDATE t SET col = ? WHERE rowid = ?` translate cleanly into
  `storage-write-dispatch.delete-rows` / `.update-rows`. Deliverable —
  the two ignored E2E tests target this extension.

### mysqlwasm-component

- **Native concept:** MySQL/MariaDB **has no universally accessible
  rowid pseudo-column**. InnoDB has an internal `DB_ROW_ID` but it is
  not SQL-selectable. `_rowid` is a per-table alias for the integer
  primary key when one exists, and NULL otherwise — not universally
  usable. Modern MySQL/MariaDB do support
  `ROW_NUMBER() OVER ()`, but the hand-rolled wire client in this
  extension talks to any server the DSN reaches (including MySQL 5.x
  without CTEs / windows).
- **table-columns:** prepend a synthetic `rowid: Int64` at index 0.
- **scan:** materialize the query into memory (already the case) and
  compute rowid = 1-based position in the emitted batch. Stable within
  one scan; the extension does not currently drive UPDATE/DELETE, so
  no cross-scan stability guarantee is claimed. This is the pattern
  the ADR labels "Postgres via a synthetic ordinal" (Amendment A5,
  line 249) — the same shape applies to mysqlwasm.
- **write path:** deferred. No E2E write tests exist against
  mysqlwasm; the write world stays unexported. Any UPDATE/DELETE
  against `<alias>.<mysqltable>` will fail cleanly at
  `at5_locate_rowid_column` -> `dispatch_storage_update_direct` with
  "storage-write-dispatch not exported". Documented v1 limitation.

### postgreswasm-component

- **Native concept:** Postgres exposes `ctid` (block + offset), a
  `tid` scalar. Not an `int8` — casting to text yields `(0,1)` and to
  int8 is illegal. Could parse it out but that couples the extension
  to Postgres's on-disk representation, which is not portable across
  heap vs. compressed vs. columnar table AMs.
- **table-columns:** prepend synthetic `rowid: Int64` at index 0.
- **scan:** row-position in the materialized batch, same as mysqlwasm.
- **write path:** deferred (no E2E tests).

### unityscan-component

- **Native concept:** Unity Catalog is a metadata service; the data
  scan itself returns EOF here (rows come from the composed
  s3fs/azfs + delta/parquet stack, not from this component). A rowid
  would only ever be exercised if the composed reader routed through
  us — it doesn't.
- **table-columns:** prepend synthetic `rowid: Int64` at index 0 for
  contract consistency (so the host's `intercept_attach` schema
  matches every storage extension's shape, even the empty ones).
- **scan:** unchanged — always EOF, so no rowid values are emitted.
- **write path:** N/A (Unity is read-only in this component).

## 5. Contract summary

An @5 storage-dispatch extension that wants to support the host's
Amendment A5 write path MUST:

1. Include a column named `rowid` (case-insensitive; canonical form
   `rowid`) with `logical: Int64` in the list returned by
   `storage-table-columns`. Any position works — the host walks the
   list — but index 0 is the shortest write-path in the host's
   `at5_prescan_rows` (one fewer `.get()`).
2. Emit an integer `duckvalue` in that column's slot for every row
   returned by `storage-scan-next`. The value MUST be uniquely
   identifying within the (catalog, table) pair for the duration of the
   pre-scan + write dispatch pair. Native store rowids (SQLite ROWID,
   Postgres native tid parsed to int, etc.) satisfy this by
   construction; a per-scan monotonic ordinal satisfies READ-only
   scenarios and defers WRITE correctness until the extension gains a
   stable native rowid.

An @5 storage-dispatch extension that DOES NOT want write support MAY:

1. Skip the rowid column entirely. `at5_locate_rowid_column` will
   return `Duckerror::Unsupported("... requires the storage extension to
   expose a 'rowid' column ...")`, which the intercept surfaces to the
   user as "UPDATE/DELETE against <alias>.<table> requires ...". READ
   path (`SELECT * FROM <alias>.<table>`) is unaffected.

We opt IN for all four extensions (per §4) because doing so has zero
cost on the read side and lets a future Phase 5 extension of the write
path (e.g., mysqlwasm gaining `ROW_NUMBER()` when the connection
reports 8.0+) drop in without a schema change on the host.
