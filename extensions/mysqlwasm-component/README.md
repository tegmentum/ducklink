# mysqlwasm-component

MySQL / MariaDB storage backend for DuckDB, compiled as a wasm component. A
minimal hand-rolled wire client (`src/mysql.rs`, plaintext + `mysql_native_
password` only) backs `ATTACH '<dsn>' (TYPE mysqlwasm)`, exposing every
server-side table through the DuckDB @5 storage-dispatch / storage-write-
dispatch WIT surface.

## Rowid mechanism (AT5 Amendment A5)

MySQL has no universally selectable rowid pseudo-column: InnoDB's internal
`DB_ROW_ID` is not SQL-exposed, `_rowid` only aliases a single-column integer
primary key, and `ROW_NUMBER() OVER ()` requires MySQL 8.0 / MariaDB 10.2+.
To stay portable across the 5.x-era servers the wire client still talks to,
the extension mints its OWN rowid and keeps the mapping in memory.

Chosen mechanism (of the three the task memo enumerated: InnoDB `_rowid`,
composed PK-as-int, or per-scan ordinal + cache):

**A per-scan monotonic ordinal cached against a WHERE-clause payload.**

Concretely:

1. `storage-table-columns` advertises a synthetic `rowid: Int64` at index 0,
   so `at5_locate_rowid_column` on the host side finds it (§4 of
   `docs/at5-rowid-mechanism.md`).
2. `storage-scan-open` ALWAYS issues `SELECT col1, col2, …, colN FROM t
   [WHERE …]` — every underlying column, regardless of the projection —
   materializes each row, mints the next per-`(catalog, table)` ordinal
   (`Catalog::next_rowid`), and stores a `(table, rowid) →
   [(col_name, value), …]` binding in `Catalog::row_map`. Only the projected
   cells are emitted to the host.
3. On UPDATE / DELETE, `storage-write-dispatch.update-rows` / `.delete-rows`
   receive the ordinal back as an `s64` rowid. The extension looks up the
   cached bindings and reconstructs `WHERE pk_col = val AND … LIMIT 1` (see
   below for the PK-vs-fallback split) as the mutation predicate.

The bindings cached per row are:

- **Tables with a `PRIMARY KEY`** — only the PK columns (fetched once via
  `SHOW KEYS FROM t WHERE Key_name = 'PRIMARY'`, cached in
  `Catalog::pk_cols`). This is the fast path: the WHERE is `pk1 = ? AND … =
  ?`, unambiguous by construction. Composite PKs work uniformly (all key
  cols land in the WHERE).
- **Tables without a `PRIMARY KEY`** — every column, matched with `LIMIT 1`
  to keep duplicates from cascading. Multiple identical rows are still
  addressable one-at-a-time: each pre-scan iteration mints a fresh ordinal,
  each write dispatch consumes exactly one rowid. Diagnostic: if the WHERE
  matches zero rows because the same value was already deleted / updated
  earlier in the same dispatch, the affected-row count under-counts. Add a
  PK to the source table if that matters.

### Trade-offs deliberately accepted

- **Extra SELECT bandwidth.** The scan fetches every column, not just the
  projected subset, so the row_map has enough data to synthesize a WHERE.
  For tall, wide tables this costs more network traffic than a projected
  SELECT. Fixable by tightening SELECT to `projected ∪ pk_cols` when a PK is
  known; kept simple at v1.
- **Ordinal is per-attach, not stable.** The rowid returned to the host is
  only meaningful until the next `storage-detach` (the `Catalog` is
  dropped and the map with it). Cross-attach caching, planning, or
  serialization of rowids is unsupported. The host uses each ordinal
  strictly within a single UPDATE / DELETE round trip, so this is invisible
  to callers.
- **PK-less tables need `LIMIT 1`.** MySQL / MariaDB support `DELETE … LIMIT
  1` and `UPDATE … LIMIT 1` on single-table statements, so this is
  well-defined; it just means writes on tables without a PK are one-row-at-
  a-time (no multi-row-affecting predicate is generated).

## Write persistence (`serialize`)

`storage-dispatch.serialize` returns `Duckerror::Unsupported`. This is
**correct** for a live-connection backend:

- Every INSERT / UPDATE / DELETE dispatched through
  `storage-write-dispatch` round-trips over the MySQL wire protocol and is
  persisted server-side before the call returns.
- There is no in-memory "database image" to hand back to the host. The DSN
  is a connection string (`mysql://…` or `host=… user=…`), not a file path.
- The host's `at5_write_back` treats `Unsupported` as "no fs write step
  needed", which is what a network-backed catalog wants. See
  `crates/ducklink-host/src/lib.rs` (`HostState::at5_write_back`).

The stub message reads: `"serialize not applicable to this backend (live
MySQL connection; mutations already persisted server-side via storage-write-
dispatch)"`.

## End-to-end tests

`crates/ducklink-host/tests/test_at5_write_mysqlwasm.rs` runs the same
INSERT / UPDATE / DELETE assertions as the sqlitewasm write suite, against a
LIVE MySQL / MariaDB server addressed by the `MYSQL_TEST_URL` environment
variable (URL form: `mysql://user:pw@host:port/db`). If `MYSQL_TEST_URL` is
unset OR the required wasm artifacts are missing, every test SKIPS cleanly
so a fresh clone's `cargo test` still passes.

Enable locally with:

    export MYSQL_TEST_URL=mysql://root:root@127.0.0.1:3306/ducktest
    export DUCKLINK_NETWORK_GRANT=mysqlwasm
    make all
    cargo component build -p mysqlwasm-component --target wasm32-wasip2 --release
    mkdir -p artifacts/extensions
    cp target/wasm32-wasip1/release/mysqlwasm.wasm artifacts/extensions/
    cargo test -p ducklink-host --test test_at5_write_mysqlwasm -- --nocapture
