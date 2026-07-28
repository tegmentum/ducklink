//! MySQL/MariaDB storage backend for DuckDB over wasi:sockets.
//!
//! A minimal hand-rolled MySQL wire-protocol client (see `mysql.rs`) connects to
//! a real server (plaintext, mysql_native_password) and serves its tables through
//! the storage / pushdown-scan WIT interface. This backs:
//!
//!     ATTACH '<dsn>' (TYPE mysql);
//!
//! where `<dsn>` is either a URL `mysql://user:pw@host:port/db` or a
//! space/`;`-separated `host=.. port=.. user=.. password=.. database=..` string.
//!
//! Network access requires the host's network grant (DUCKLINK_NETWORK_GRANT).
//! Nothing panics across the FFI boundary -- every failure maps to a duckerror.
use std::cell::RefCell;
use std::collections::HashMap;

use wit_bindgen::rt::string::String;
use wit_bindgen::rt::vec::Vec;

mod mysql;
use mysql::{ColType, Column, MyConn};

wit_bindgen::generate!({ path: "./wit", world: "duckdb:extension/duckdb-extension-storage-write" });

use duckdb::extension::{storage, types};
use exports::duckdb::extension::{callback_dispatch, guest, storage_dispatch, storage_write_dispatch};

/// Opaque callback handle the host passes back to every storage-dispatch call.
const STORAGE_HANDLE: u32 = 1;

struct Extension;

impl guest::Guest for Extension {
    fn load() -> Result<types::Loadresult, types::Duckerror> {
        // Register the backend keyed by the ATTACH TYPE name "mysql". Also
        // register an alias "mysqlwasm" that cannot collide with the core's
        // native mysql_scanner StorageExtension (useful when the lean core
        // happens to ship the native scanner).
        storage::register_storage("mysql", STORAGE_HANDLE, None)?;
        storage::register_storage("mysqlwasm", STORAGE_HANDLE, None)?;
        Ok(types::Loadresult {
            name: "mysqlwasm".into(),
            version: Some(env!("CARGO_PKG_VERSION").into()),
            requires: Vec::new().into(),
        })
    }
    fn reconfigure(_k: Vec<String>) -> Result<bool, types::Duckerror> {
        Ok(false)
    }
    fn shutdown() -> Result<bool, types::Duckerror> {
        Ok(false)
    }
}

// This backend has no functions; the callback-dispatch export is required by the
// world but every entry is unsupported.
impl callback_dispatch::Guest for Extension {
    // major-4 columnar hot path: this storage backend has no scalar/aggregate/
    // cast functions, so the three columnar methods are Unsupported stubs.
    datalink_extcore::columnar_stub!();

    fn call_scalar(
        _h: u32,
        _a: Vec<types::Duckvalue>,
        _c: types::Invokeinfo,
    ) -> Result<types::Duckvalue, types::Duckerror> {
        Err(types::Duckerror::Unsupported("mysql: no scalar fns".into()))
    }
    fn call_table(
        _h: u32,
        _args: Vec<types::Duckvalue>,
    ) -> Result<types::Resultset, types::Duckerror> {
        Err(types::Duckerror::Unsupported("mysql: no table fns".into()))
    }
    fn call_pragma(
        _h: u32,
        _a: Vec<types::Duckvalue>,
    ) -> Result<Option<types::Duckvalue>, types::Duckerror> {
        Err(types::Duckerror::Unsupported("mysql: no pragmas".into()))
    }
    fn call_cast(
        _h: u32,
        _v: types::Duckvalue,
    ) -> Result<types::Duckvalue, types::Duckerror> {
        Err(types::Duckerror::Unsupported("mysql: no casts".into()))
    }
}

// ---------------------------------------------------------------------------
// storage-dispatch
// ---------------------------------------------------------------------------

/// Per-catalog state: a live connection plus a cache of table -> column list
/// (so a scan can resolve projection/filter indices to column names without a
/// round trip) plus the AT5 rowid infrastructure.
///
/// AT5 rowid model (Amendment A5, docs/at5-rowid-mechanism.md): MySQL has no
/// universally selectable rowid pseudo-column, so the extension emits a
/// per-scan monotonic ordinal in the synthetic rowid slot and caches the
/// row-identifying values it fetched alongside. `row_map` is that cache:
/// `(table, rowid) -> [(col_name, value), ...]` binds each emitted rowid to
/// enough underlying-column values to rebuild a WHERE clause for UPDATE /
/// DELETE. When the table has a PRIMARY KEY, only the PK cols are cached
/// (single-column index); otherwise every column is cached and writes fall
/// back to a whole-row match with `LIMIT 1` (duplicates target one row per
/// dispatched rowid).
struct Catalog {
    conn: MyConn,
    columns: HashMap<std::string::String, std::vec::Vec<Column>>,
    /// Table -> indices (into `columns[table]`) of the PRIMARY KEY columns,
    /// in `Seq_in_index` order. Empty vec = no PK detected.
    pk_cols: HashMap<std::string::String, std::vec::Vec<usize>>,
    /// (table, rowid) -> WHERE-clause bindings (see struct doc-comment).
    row_map: HashMap<(std::string::String, i64), std::vec::Vec<(std::string::String, types::Duckvalue)>>,
    /// Per-table monotonic counter used both to mint rowid values and as the
    /// key into `row_map`. Never wraps within a catalog's lifetime.
    next_rowid: HashMap<std::string::String, i64>,
}

struct Cursor {
    rows: std::vec::Vec<std::vec::Vec<types::Duckvalue>>,
    pos: usize,
}

thread_local! {
    static CATALOGS: RefCell<HashMap<u32, Catalog>> = RefCell::new(HashMap::new());
    static SCANS: RefCell<HashMap<u32, Cursor>> = RefCell::new(HashMap::new());
    /// Open write-transactions -> catalog-id (mirrors sqlitewasm). We don't
    /// wrap `MyConn` in a transaction object -- MySQL's server-side txn state
    /// lives on the connection, so `catalog` is a sufficient key.
    static TXNS: RefCell<HashMap<u32, u32>> = RefCell::new(HashMap::new());
    static NEXT_CATALOG: RefCell<u32> = const { RefCell::new(1) };
    static NEXT_SCAN: RefCell<u32> = const { RefCell::new(1) };
    static NEXT_TXN: RefCell<u32> = const { RefCell::new(1) };
}

impl storage_dispatch::Guest for Extension {
    /// Not used for mysql (the dsn is a connection string, not a file blob).
    fn attach_blob(
        handle: u32,
        _dsn: String,
        _bytes: Vec<u8>,
    ) -> Result<(), types::Duckerror> {
        check_handle(handle)?;
        Ok(())
    }

    fn storage_attach(
        handle: u32,
        dsn: String,
        _options: Vec<(String, String)>,
    ) -> Result<u32, types::Duckerror> {
        check_handle(handle)?;
        let cfg = parse_dsn(&dsn)?;
        let conn = MyConn::connect(
            &cfg.host,
            cfg.port,
            &cfg.user,
            &cfg.password,
            &cfg.database,
        )
        .map_err(|e| types::Duckerror::Io(format!("mysql attach: {}", e.0)))?;
        let id = NEXT_CATALOG.with(|n| {
            let mut n = n.borrow_mut();
            let id = *n;
            *n += 1;
            id
        });
        CATALOGS.with(|c| {
            c.borrow_mut().insert(
                id,
                Catalog {
                    conn,
                    columns: HashMap::new(),
                    pk_cols: HashMap::new(),
                    row_map: HashMap::new(),
                    next_rowid: HashMap::new(),
                },
            )
        });
        Ok(id)
    }

    fn storage_list_tables(
        handle: u32,
        catalog: u32,
    ) -> Result<Vec<String>, types::Duckerror> {
        check_handle(handle)?;
        with_catalog(catalog, |cat| {
            let rs = cat
                .conn
                .query("SHOW TABLES")
                .map_err(|e| types::Duckerror::Io(format!("SHOW TABLES: {}", e.0)))?;
            let mut out: Vec<String> = Vec::new();
            for row in &rs.rows {
                if let Some(Some(name)) = row.first() {
                    out.push(name.clone().into());
                }
            }
            Ok(out)
        })
    }

    fn storage_table_columns(
        handle: u32,
        catalog: u32,
        table: String,
    ) -> Result<Vec<types::Columndef>, types::Duckerror> {
        check_handle(handle)?;
        with_catalog(catalog, |cat| {
            let cols = fetch_columns(cat, &table)?;
            // ADR Amendment A5: prepend a synthetic `rowid` (Int64) column at
            // index 0. MySQL has no universally-selectable rowid pseudo-column,
            // so the scan emits a per-scan monotonic ordinal as the rowid
            // value and the extension caches the row-identifying column values
            // (PK cols, or full row for PK-less tables) in `Catalog::row_map`
            // to translate rowids back into WHERE clauses for UPDATE / DELETE.
            // See docs/at5-rowid-mechanism.md + this crate's README for the
            // full mechanism.
            let mut out: Vec<types::Columndef> = Vec::with_capacity(cols.len() + 1);
            out.push(types::Columndef {
                name: "rowid".into(),
                logical: types::Logicaltype::Int64,
            });
            for c in cols {
                out.push(types::Columndef {
                    name: c.name.clone().into(),
                    logical: coltype_to_logical(c.ty),
                });
            }
            Ok(out)
        })
    }

    fn storage_scan_open(
        handle: u32,
        catalog: u32,
        request: storage::ScanRequest,
    ) -> Result<u32, types::Duckerror> {
        check_handle(handle)?;
        let rows = with_catalog(catalog, |cat| run_scan(cat, &request))?;
        let id = NEXT_SCAN.with(|n| {
            let mut n = n.borrow_mut();
            let id = *n;
            *n += 1;
            id
        });
        SCANS.with(|s| s.borrow_mut().insert(id, Cursor { rows, pos: 0 }));
        Ok(id)
    }

    fn storage_scan_next(
        handle: u32,
        scan: u32,
        max_rows: u32,
    ) -> Result<types::Resultset, types::Duckerror> {
        check_handle(handle)?;
        SCANS.with(|s| {
            let mut s = s.borrow_mut();
            let cur = s
                .get_mut(&scan)
                .ok_or_else(|| types::Duckerror::Invalidstate("unknown scan".into()))?;
            // DEFENSIVE (wasm32): `cur.pos + max_rows as usize` can wrap
            // when `max_rows == u32::MAX` (or a caller passes a huge sentinel
            // meaning "give me everything") because `usize` is 32-bit under
            // wasm. `saturating_add` clamps to `usize::MAX` so the subsequent
            // `.min(cur.rows.len())` collapses to `cur.rows.len()`. Also
            // clamp `cur.pos` to `cur.rows.len()` in case a prior call left
            // it past the end (e.g. rows re-computed to be shorter -- not
            // possible today, but this method is called across scan lifetimes
            // and safety-first here is cheap).
            let len = cur.rows.len();
            let start = cur.pos.min(len);
            let end = start.saturating_add(max_rows as usize).min(len);
            let batch: std::vec::Vec<std::vec::Vec<types::Duckvalue>> =
                cur.rows[start..end].to_vec();
            cur.pos = end;
            Ok(batch.into())
        })
    }

    fn storage_scan_close(handle: u32, scan: u32) -> Result<bool, types::Duckerror> {
        check_handle(handle)?;
        SCANS.with(|s| s.borrow_mut().remove(&scan));
        Ok(true)
    }

    fn storage_detach(handle: u32, catalog: u32) -> Result<bool, types::Duckerror> {
        check_handle(handle)?;
        // Dropping the Catalog drops the TcpStream, closing the connection.
        CATALOGS.with(|c| c.borrow_mut().remove(&catalog));
        Ok(true)
    }

    /// AT5 write-back stub: MySQL is a LIVE network connection, not a
    /// serializable file blob. The write path already persisted every
    /// mutation over the wire via `storage-write-dispatch` (INSERT / UPDATE /
    /// DELETE round-trip through `MyConn::query`), so there is no "in-memory
    /// image" for the host to re-emit onto disk. Returning `Unsupported` tells
    /// `HostState::at5_write_back` to silently skip the fs write step, which
    /// is the correct semantics for a live-connection backend. See the
    /// crate README + docs/at5-rowid-mechanism.md.
    fn serialize(handle: u32, _catalog: u32) -> Result<Vec<u8>, types::Duckerror> {
        check_handle(handle)?;
        Err(types::Duckerror::Unsupported(
            "serialize not applicable to this backend (live MySQL connection; \
             mutations already persisted server-side via storage-write-dispatch)"
                .into(),
        ))
    }
}

// ---------------------------------------------------------------------------
// storage-write-dispatch (ADR Amendment A5)
//
// The read side (storage-dispatch) emits a per-scan monotonic ordinal in the
// rowid slot and caches WHERE-clause bindings in `Catalog::row_map`. Every
// write call below (delete-rows / update-rows) looks the rowid up in that
// map and reconstructs `WHERE pk_col = val AND ... LIMIT 1` to route the
// mutation to a single server-side row. Tables with a PK use their PK cols
// as the WHERE; PK-less tables fall back to matching every column (still
// LIMIT 1). See docs/at5-rowid-mechanism.md.
// ---------------------------------------------------------------------------

impl storage_write_dispatch::Guest for Extension {
    fn begin_transaction(handle: u32, catalog: u32) -> Result<u32, types::Duckerror> {
        check_handle(handle)?;
        with_catalog(catalog, |cat| {
            cat.conn
                .query("START TRANSACTION")
                .map_err(|e| types::Duckerror::Io(format!("START TRANSACTION: {}", e.0)))?;
            Ok(())
        })?;
        let id = NEXT_TXN.with(|n| {
            let mut n = n.borrow_mut();
            let id = *n;
            *n += 1;
            id
        });
        TXNS.with(|t| t.borrow_mut().insert(id, catalog));
        Ok(id)
    }

    fn commit_transaction(handle: u32, txn: u32) -> Result<(), types::Duckerror> {
        check_handle(handle)?;
        let catalog = take_txn(txn)?;
        with_catalog(catalog, |cat| {
            cat.conn
                .query("COMMIT")
                .map_err(|e| types::Duckerror::Io(format!("COMMIT: {}", e.0)))?;
            Ok(())
        })
    }

    fn rollback_transaction(handle: u32, txn: u32) -> Result<(), types::Duckerror> {
        check_handle(handle)?;
        let catalog = take_txn(txn)?;
        with_catalog(catalog, |cat| {
            cat.conn
                .query("ROLLBACK")
                .map_err(|e| types::Duckerror::Io(format!("ROLLBACK: {}", e.0)))?;
            Ok(())
        })
    }

    /// Bug 4 write-back probe (see storage-write-dispatch.wit).
    ///
    /// MySQL persists every INSERT / UPDATE / DELETE over the wire at dispatch
    /// time -- the server IS the store of record. There is no in-wasm database
    /// image the host could serialize back to a file, so return `false` and let
    /// the host's default (serialize + fs write-back) skip via the `serialize`
    /// stub's `Unsupported` return. Returning `true` here would (incorrectly)
    /// invite the host to open a "native, file-backed connection to the DSN",
    /// which is meaningless for a remote server URL.
    fn writes_persist_directly(handle: u32) -> Result<bool, types::Duckerror> {
        check_handle(handle)?;
        Ok(false)
    }

    fn create_table(
        handle: u32,
        txn: u32,
        table: String,
        columns: Vec<types::Columndef>,
    ) -> Result<(), types::Duckerror> {
        check_handle(handle)?;
        let catalog = txn_catalog(txn)?;
        let col_defs: std::vec::Vec<std::string::String> = columns
            .iter()
            .map(|c| format!("{} {}", backtick(&c.name), logical_to_mysql(&c.logical)))
            .collect();
        let sql = format!(
            "CREATE TABLE {} ({})",
            backtick(&table),
            col_defs.join(", ")
        );
        with_catalog(catalog, |cat| {
            cat.conn
                .query(&sql)
                .map_err(|e| types::Duckerror::Io(format!("CREATE TABLE: {}", e.0)))?;
            Ok(())
        })
    }

    fn insert_rows(
        handle: u32,
        txn: u32,
        table: String,
        rows: Vec<Vec<types::Duckvalue>>,
    ) -> Result<u64, types::Duckerror> {
        check_handle(handle)?;
        let catalog = txn_catalog(txn)?;
        if rows.is_empty() {
            return Ok(0);
        }
        with_catalog(catalog, |cat| {
            let cols = fetch_columns(cat, &table)?.clone();
            let n_underlying = cols.len();
            let width = rows[0].len();
            // Rows arrive at the extension's advertised width -- either the
            // bare underlying columns or `[rowid, col1, col2, ...]` with the
            // synthetic rowid at index 0 (Amendment A5). Strip the rowid when
            // present: an INSERT lets MySQL mint any auto-increment PK; we do
            // not carry the synthetic ordinal across the boundary.
            let strip_rowid = width == n_underlying + 1;
            if !strip_rowid && width != n_underlying {
                return Err(types::Duckerror::Invalidargument(format!(
                    "insert-rows into '{table}': expected {n_underlying} or \
                     {} values per row, got {width}",
                    n_underlying + 1
                )));
            }
            let col_list: std::vec::Vec<std::string::String> =
                cols.iter().map(|c| backtick(&c.name)).collect();
            let mut inserted: u64 = 0;
            for row in &rows {
                let payload: &[types::Duckvalue] = if strip_rowid { &row[1..] } else { &row[..] };
                let vals: std::vec::Vec<std::string::String> =
                    payload.iter().map(literal).collect();
                let sql = format!(
                    "INSERT INTO {} ({}) VALUES ({})",
                    backtick(&table),
                    col_list.join(", "),
                    vals.join(", ")
                );
                cat.conn
                    .query(&sql)
                    .map_err(|e| types::Duckerror::Io(format!("INSERT: {}", e.0)))?;
                inserted += 1;
            }
            Ok(inserted)
        })
    }

    fn delete_rows(
        handle: u32,
        txn: u32,
        table: String,
        rowids: Vec<i64>,
    ) -> Result<u64, types::Duckerror> {
        check_handle(handle)?;
        let catalog = txn_catalog(txn)?;
        if rowids.is_empty() {
            return Ok(0);
        }
        with_catalog(catalog, |cat| {
            let mut deleted: u64 = 0;
            for &rid in &rowids {
                let bindings = cat
                    .row_map
                    .get(&(table.to_string(), rid))
                    .cloned()
                    .ok_or_else(|| {
                        types::Duckerror::Internal(format!(
                            "delete-rows for '{table}': rowid {rid} not in row_map \
                             (scan must precede write; each attach caches its own map)"
                        ))
                    })?;
                let where_sql = build_where_clause(&bindings);
                let sql = format!(
                    "DELETE FROM {} WHERE {} LIMIT 1",
                    backtick(&table),
                    where_sql
                );
                cat.conn
                    .query(&sql)
                    .map_err(|e| types::Duckerror::Io(format!("DELETE: {}", e.0)))?;
                deleted += 1;
            }
            Ok(deleted)
        })
    }

    fn update_rows(
        handle: u32,
        txn: u32,
        table: String,
        rowids: Vec<i64>,
        rows: Vec<Vec<types::Duckvalue>>,
    ) -> Result<u64, types::Duckerror> {
        check_handle(handle)?;
        let catalog = txn_catalog(txn)?;
        if rowids.len() != rows.len() {
            return Err(types::Duckerror::Invalidargument(format!(
                "update-rows for '{table}': {} rowids vs {} row payloads",
                rowids.len(),
                rows.len()
            )));
        }
        if rowids.is_empty() {
            return Ok(0);
        }
        with_catalog(catalog, |cat| {
            let cols = fetch_columns(cat, &table)?.clone();
            let n_underlying = cols.len();
            let width = rows[0].len();
            // Same width contract as insert-rows: rowid-prefixed or bare.
            let strip_rowid = width == n_underlying + 1;
            if !strip_rowid && width != n_underlying {
                return Err(types::Duckerror::Invalidargument(format!(
                    "update-rows for '{table}': expected {n_underlying} or \
                     {} values per row, got {width}",
                    n_underlying + 1
                )));
            }
            let mut updated: u64 = 0;
            for (rid, row) in rowids.iter().zip(rows.iter()) {
                let bindings = cat
                    .row_map
                    .get(&(table.to_string(), *rid))
                    .cloned()
                    .ok_or_else(|| {
                        types::Duckerror::Internal(format!(
                            "update-rows for '{table}': rowid {rid} not in row_map \
                             (scan must precede write; each attach caches its own map)"
                        ))
                    })?;
                let where_sql = build_where_clause(&bindings);
                let payload: &[types::Duckvalue] = if strip_rowid { &row[1..] } else { &row[..] };
                let set_list: std::vec::Vec<std::string::String> = cols
                    .iter()
                    .zip(payload.iter())
                    .map(|(c, v)| format!("{} = {}", backtick(&c.name), literal(v)))
                    .collect();
                let sql = format!(
                    "UPDATE {} SET {} WHERE {} LIMIT 1",
                    backtick(&table),
                    set_list.join(", "),
                    where_sql
                );
                cat.conn
                    .query(&sql)
                    .map_err(|e| types::Duckerror::Io(format!("UPDATE: {}", e.0)))?;
                updated += 1;
            }
            Ok(updated)
        })
    }
}

/// Look up a live txn's catalog (leaves the map entry in place; the caller
/// takes it via `take_txn` on commit/rollback).
fn txn_catalog(txn: u32) -> Result<u32, types::Duckerror> {
    TXNS.with(|t| {
        t.borrow()
            .get(&txn)
            .copied()
            .ok_or_else(|| types::Duckerror::Invalidstate(format!("unknown txn {txn}")))
    })
}

fn take_txn(txn: u32) -> Result<u32, types::Duckerror> {
    TXNS.with(|t| {
        t.borrow_mut()
            .remove(&txn)
            .ok_or_else(|| types::Duckerror::Invalidstate(format!("unknown txn {txn}")))
    })
}

/// Build an AND-joined WHERE clause from row_map bindings. NULL cells use
/// `IS NULL`; every other cell renders via `literal(v)`.
fn build_where_clause(
    bindings: &[(std::string::String, types::Duckvalue)],
) -> std::string::String {
    bindings
        .iter()
        .map(|(col, val)| match val {
            types::Duckvalue::Null => format!("{} IS NULL", backtick(col)),
            _ => format!("{} = {}", backtick(col), literal(val)),
        })
        .collect::<std::vec::Vec<_>>()
        .join(" AND ")
}

/// Map an extension logicaltype to a MySQL column type name for CREATE TABLE.
fn logical_to_mysql(ty: &types::Logicaltype) -> &'static str {
    match ty {
        types::Logicaltype::Boolean => "TINYINT(1)",
        types::Logicaltype::Int8 | types::Logicaltype::Uint8 => "TINYINT",
        types::Logicaltype::Int16 | types::Logicaltype::Uint16 => "SMALLINT",
        types::Logicaltype::Int32 | types::Logicaltype::Uint32 => "INT",
        types::Logicaltype::Int64 | types::Logicaltype::Uint64 => "BIGINT",
        types::Logicaltype::Float32 => "FLOAT",
        types::Logicaltype::Float64 => "DOUBLE",
        types::Logicaltype::Blob => "BLOB",
        _ => "TEXT",
    }
}

fn check_handle(handle: u32) -> Result<(), types::Duckerror> {
    if handle == STORAGE_HANDLE {
        Ok(())
    } else {
        Err(types::Duckerror::Internal("unknown storage handle".into()))
    }
}

/// Run `f` with mutable access to the catalog identified by `id`.
fn with_catalog<T>(
    id: u32,
    f: impl FnOnce(&mut Catalog) -> Result<T, types::Duckerror>,
) -> Result<T, types::Duckerror> {
    CATALOGS.with(|c| {
        let mut c = c.borrow_mut();
        let cat = c
            .get_mut(&id)
            .ok_or_else(|| types::Duckerror::Invalidstate("unknown catalog".into()))?;
        f(cat)
    })
}

/// Resolve a table's columns, caching the result on the catalog.
fn fetch_columns<'a>(
    cat: &'a mut Catalog,
    table: &str,
) -> Result<&'a std::vec::Vec<Column>, types::Duckerror> {
    if !cat.columns.contains_key(table) {
        let sql = format!("SHOW COLUMNS FROM {}", backtick(table));
        let rs = cat
            .conn
            .query(&sql)
            .map_err(|e| types::Duckerror::Io(format!("SHOW COLUMNS: {}", e.0)))?;
        // SHOW COLUMNS rows: Field, Type, Null, Key, Default, Extra.
        let mut cols = std::vec::Vec::with_capacity(rs.rows.len());
        for row in &rs.rows {
            let name = match row.first() {
                Some(Some(n)) => n.clone(),
                _ => continue,
            };
            let type_text = match row.get(1) {
                Some(Some(t)) => t.as_str(),
                _ => "",
            };
            cols.push(Column {
                name,
                ty: classify_decl(type_text),
            });
        }
        if cols.is_empty() {
            return Err(types::Duckerror::Invalidargument(format!(
                "table '{table}' not found or has no columns"
            )));
        }
        cat.columns.insert(table.to_string(), cols);
    }
    Ok(cat.columns.get(table).unwrap())
}

/// Build + run the pushdown query, materializing rows mapped to logical types
/// and populating the AT5 rowid map for any subsequent UPDATE / DELETE.
///
/// Unlike a "SELECT only the projected columns" implementation, this scan
/// ALWAYS fetches every underlying column (`SELECT c1, c2, ..., cN FROM t`)
/// regardless of `request.projection`. The extra cells are needed to seed
/// `Catalog::row_map` so that a rowid dispatched back through the write
/// path can be resolved to a WHERE clause. Emission still honors the
/// projection -- only the requested cells leave the extension.
fn run_scan(
    cat: &mut Catalog,
    request: &storage::ScanRequest,
) -> Result<std::vec::Vec<std::vec::Vec<types::Duckvalue>>, types::Duckerror> {
    let cols: std::vec::Vec<Column> = fetch_columns(cat, &request.table)?.clone();
    let pk_indices: std::vec::Vec<usize> = fetch_pk_indices(cat, &request.table)?;
    // Host-side indices are 1-based into MySQL's columns: index 0 refers to
    // the synthetic `rowid` slot the extension advertises in
    // `storage_table_columns` (ADR Amendment A5). n_host_cols includes it.
    let n_host_cols = cols.len() + 1;

    // Projection: indices into the synthetic column list, in emit order.
    // Empty = all columns (including rowid).
    let proj: std::vec::Vec<usize> = if request.projection.is_empty() {
        (0..n_host_cols).collect()
    } else {
        request.projection.iter().map(|&i| i as usize).collect()
    };
    for &i in &proj {
        if i >= n_host_cols {
            return Err(types::Duckerror::Invalidargument(
                "projection index out of range".into(),
            ));
        }
    }

    // Always fetch every underlying column, in declared order, so `row_map`
    // can bind the row-identifying values. The rowid slot itself is
    // materialized post-fetch (a per-scan monotonic ordinal); MySQL has no
    // sql-selectable rowid pseudo-column.
    let select_list: std::vec::Vec<std::string::String> =
        cols.iter().map(|c| backtick(&c.name)).collect();
    let mut sql = format!(
        "SELECT {} FROM {}",
        select_list.join(", "),
        backtick(&request.table)
    );

    // WHERE: AND-join the filters, binding values inline (numbers raw, strings
    // single-quoted with '' escaping). Filters on the synthetic rowid column
    // (index 0) are declined at the SQL layer -- the ordinal is minted
    // post-fetch and has no server-side counterpart. The host re-applies
    // filters above the scan, so declining pushdown for rowid is safe.
    let mut conds: std::vec::Vec<std::string::String> = std::vec::Vec::new();
    for fltr in &request.filters {
        let idx = fltr.column as usize;
        if idx >= n_host_cols {
            return Err(types::Duckerror::Invalidargument(
                "filter column index out of range".into(),
            ));
        }
        if idx == 0 {
            // Rowid filter: decline pushdown; the host re-applies filters.
            continue;
        }
        let col = backtick(&cols[idx - 1].name);
        match fltr.op {
            storage::CompareOp::IsNull => conds.push(format!("{col} IS NULL")),
            storage::CompareOp::IsNotNull => conds.push(format!("{col} IS NOT NULL")),
            op => {
                let sym = match op {
                    storage::CompareOp::Eq => "=",
                    storage::CompareOp::Ne => "<>",
                    storage::CompareOp::Lt => "<",
                    storage::CompareOp::Le => "<=",
                    storage::CompareOp::Gt => ">",
                    storage::CompareOp::Ge => ">=",
                    _ => unreachable!(),
                };
                conds.push(format!("{col} {sym} {}", literal(&fltr.value)));
            }
        }
    }
    if !conds.is_empty() {
        sql.push_str(" WHERE ");
        sql.push_str(&conds.join(" AND "));
    }

    if let Some(n) = request.limit {
        sql.push_str(&format!(" LIMIT {n}"));
    }

    let rs = cat
        .conn
        .query(&sql)
        .map_err(|e| types::Duckerror::Io(format!("scan query: {}", e.0)))?;

    let mut out: std::vec::Vec<std::vec::Vec<types::Duckvalue>> =
        std::vec::Vec::with_capacity(rs.rows.len());
    let table_key = request.table.to_string();
    for row in &rs.rows {
        // Materialize every underlying column, in declared order, so the
        // rowid map can index into a stable per-row vector.
        let materialized: std::vec::Vec<types::Duckvalue> = cols
            .iter()
            .enumerate()
            .map(|(i, c)| text_to_duck(row.get(i).and_then(|v| v.as_ref()), c.ty))
            .collect();

        // Mint the next per-(catalog, table) rowid.
        let rid = {
            let ctr = cat
                .next_rowid
                .entry(table_key.clone())
                .or_insert(0);
            *ctr += 1;
            *ctr
        };

        // Populate the rowid map: PK cols when the table has a PK (compact
        // WHERE), otherwise the full column set (fallback WHERE with LIMIT 1
        // in the write dispatch).
        let bindings: std::vec::Vec<(std::string::String, types::Duckvalue)> =
            if !pk_indices.is_empty() {
                pk_indices
                    .iter()
                    .map(|&i| (cols[i].name.clone(), materialized[i].clone()))
                    .collect()
            } else {
                cols.iter()
                    .enumerate()
                    .map(|(i, c)| (c.name.clone(), materialized[i].clone()))
                    .collect()
            };
        cat.row_map.insert((table_key.clone(), rid), bindings);

        // Emit only the projected cells.
        let mut emit: std::vec::Vec<types::Duckvalue> = std::vec::Vec::with_capacity(proj.len());
        for &host_idx in &proj {
            if host_idx == 0 {
                emit.push(types::Duckvalue::Int64(rid));
            } else {
                emit.push(materialized[host_idx - 1].clone());
            }
        }
        out.push(emit);
    }
    Ok(out)
}

/// Fetch the ordered PRIMARY KEY column indices for `table` (indices into the
/// list returned by `fetch_columns`). Empty vec = no PK. Cached per-catalog.
///
/// Uses `SHOW KEYS FROM t WHERE Key_name = 'PRIMARY'`, which is universal
/// across MySQL 5.x/8.x and MariaDB. `Column_name` lives at index 4 of the
/// result set (0-based); `Seq_in_index` at index 3.
fn fetch_pk_indices(
    cat: &mut Catalog,
    table: &str,
) -> Result<std::vec::Vec<usize>, types::Duckerror> {
    if !cat.pk_cols.contains_key(table) {
        let cols = fetch_columns(cat, table)?.clone();
        let sql = format!(
            "SHOW KEYS FROM {} WHERE Key_name = 'PRIMARY'",
            backtick(table)
        );
        let rs = cat
            .conn
            .query(&sql)
            .map_err(|e| types::Duckerror::Io(format!("SHOW KEYS: {}", e.0)))?;
        // Sort by Seq_in_index (parsed as an int; skip on parse failure).
        let mut pk_names: std::vec::Vec<(u32, std::string::String)> = std::vec::Vec::new();
        for row in &rs.rows {
            let seq = row
                .get(3)
                .and_then(|c| c.as_ref())
                .and_then(|s| s.parse::<u32>().ok())
                .unwrap_or(0);
            if let Some(Some(name)) = row.get(4) {
                pk_names.push((seq, name.clone()));
            }
        }
        pk_names.sort_by_key(|(s, _)| *s);
        let pk_indices: std::vec::Vec<usize> = pk_names
            .into_iter()
            .filter_map(|(_, name)| cols.iter().position(|c| c.name == name))
            .collect();
        cat.pk_cols.insert(table.to_string(), pk_indices);
    }
    Ok(cat.pk_cols.get(table).unwrap().clone())
}

// ---------------------------------------------------------------------------
// value + type mapping
// ---------------------------------------------------------------------------

fn coltype_to_logical(ty: ColType) -> types::Logicaltype {
    match ty {
        ColType::Int => types::Logicaltype::Int64,
        ColType::Float => types::Logicaltype::Float64,
        ColType::Text => types::Logicaltype::Text,
    }
}

/// Classify a SHOW COLUMNS `Type` text (e.g. "int(11)", "double", "varchar(20)").
fn classify_decl(decl: &str) -> ColType {
    let d = decl.trim().to_ascii_lowercase();
    if d.starts_with("int")
        || d.starts_with("tinyint")
        || d.starts_with("smallint")
        || d.starts_with("mediumint")
        || d.starts_with("bigint")
        || d.starts_with("year")
    {
        ColType::Int
    } else if d.starts_with("float")
        || d.starts_with("double")
        || d.starts_with("decimal")
        || d.starts_with("numeric")
        || d.starts_with("real")
    {
        ColType::Float
    } else {
        ColType::Text
    }
}

/// Values arrive as text; parse toward the column's logical type, with a clean
/// fallback to text when parsing fails. SQL NULL -> Null.
fn text_to_duck(cell: Option<&std::string::String>, ty: ColType) -> types::Duckvalue {
    let s = match cell {
        None => return types::Duckvalue::Null,
        Some(s) => s,
    };
    match ty {
        ColType::Int => match s.parse::<i64>() {
            Ok(v) => types::Duckvalue::Int64(v),
            Err(_) => types::Duckvalue::Text(s.clone().into()),
        },
        ColType::Float => match s.parse::<f64>() {
            Ok(v) => types::Duckvalue::Float64(v),
            Err(_) => types::Duckvalue::Text(s.clone().into()),
        },
        ColType::Text => types::Duckvalue::Text(s.clone().into()),
    }
}

/// Render a duckvalue as an inline SQL literal (minimal escaping).
fn literal(v: &types::Duckvalue) -> std::string::String {
    match v {
        types::Duckvalue::Null => "NULL".to_string(),
        types::Duckvalue::Boolean(b) => if *b { "1" } else { "0" }.to_string(),
        types::Duckvalue::Int8(i) => i.to_string(),
        types::Duckvalue::Int16(i) => i.to_string(),
        types::Duckvalue::Int32(i) => i.to_string(),
        types::Duckvalue::Int64(i) => i.to_string(),
        types::Duckvalue::Uint8(u) => u.to_string(),
        types::Duckvalue::Uint16(u) => u.to_string(),
        types::Duckvalue::Uint32(u) => u.to_string(),
        types::Duckvalue::Uint64(u) => u.to_string(),
        types::Duckvalue::Float32(f) => f.to_string(),
        types::Duckvalue::Float64(f) => f.to_string(),
        types::Duckvalue::Text(s) => quote_str(s),
        types::Duckvalue::Blob(b) => quote_str(&String::from_utf8_lossy(b)),
        // Temporal / exotic types have no inline-literal form here; render their
        // debug text quoted so a filter bind never panics on an unhandled arm.
        other => quote_str(&std::format!("{other:?}")),
    }
}

/// Single-quote a string literal, doubling embedded single quotes and escaping
/// backslashes (MySQL treats `\` as an escape char by default).
fn quote_str(s: &str) -> std::string::String {
    let escaped = s.replace('\\', "\\\\").replace('\'', "''");
    format!("'{escaped}'")
}

/// Backtick-quote an identifier, doubling embedded backticks.
fn backtick(name: &str) -> std::string::String {
    format!("`{}`", name.replace('`', "``"))
}

// ---------------------------------------------------------------------------
// DSN parsing
// ---------------------------------------------------------------------------

struct DsnConfig {
    host: std::string::String,
    port: u16,
    user: std::string::String,
    password: std::string::String,
    database: std::string::String,
}

/// Parse a connection string. Supports a URL form
/// `mysql://user:pw@host:port/db` and a key=value form
/// `host=.. port=.. user=.. password=.. database=..` (whitespace or `;`
/// separated). Defaults: host 127.0.0.1, port 3306.
fn parse_dsn(dsn: &str) -> Result<DsnConfig, types::Duckerror> {
    let dsn = dsn.trim();
    let mut cfg = DsnConfig {
        host: "127.0.0.1".to_string(),
        port: 3306,
        user: std::string::String::new(),
        password: std::string::String::new(),
        database: std::string::String::new(),
    };

    if let Some(rest) = dsn
        .strip_prefix("mysql://")
        .or_else(|| dsn.strip_prefix("mariadb://"))
    {
        // [user[:pw]@]host[:port][/db]
        let (authority, db) = match rest.split_once('/') {
            Some((a, d)) => (a, Some(d)),
            None => (rest, None),
        };
        let (userinfo, hostport) = match authority.rsplit_once('@') {
            Some((u, h)) => (Some(u), h),
            None => (None, authority),
        };
        if let Some(ui) = userinfo {
            match ui.split_once(':') {
                Some((u, p)) => {
                    cfg.user = u.to_string();
                    cfg.password = p.to_string();
                }
                None => cfg.user = ui.to_string(),
            }
        }
        if !hostport.is_empty() {
            match hostport.rsplit_once(':') {
                Some((h, p)) => {
                    if !h.is_empty() {
                        cfg.host = h.to_string();
                    }
                    cfg.port = p
                        .parse()
                        .map_err(|_| types::Duckerror::Invalidargument(format!("bad port '{p}'")))?;
                }
                None => cfg.host = hostport.to_string(),
            }
        }
        if let Some(d) = db {
            cfg.database = d.to_string();
        }
    } else {
        // key=value form, separated by whitespace and/or ';'.
        for tok in dsn.split(|c: char| c.is_whitespace() || c == ';') {
            let tok = tok.trim();
            if tok.is_empty() {
                continue;
            }
            let (k, val) = tok
                .split_once('=')
                .ok_or_else(|| types::Duckerror::Invalidargument(format!("bad dsn token '{tok}'")))?;
            let val = val.to_string();
            match k.trim().to_ascii_lowercase().as_str() {
                "host" | "hostname" | "server" => cfg.host = val,
                "port" => {
                    cfg.port = val.parse().map_err(|_| {
                        types::Duckerror::Invalidargument(format!("bad port '{val}'"))
                    })?
                }
                "user" | "username" | "uid" => cfg.user = val,
                "password" | "passwd" | "pwd" => cfg.password = val,
                "database" | "dbname" | "db" => cfg.database = val,
                _ => { /* ignore unknown keys */ }
            }
        }
    }

    if cfg.user.is_empty() {
        return Err(types::Duckerror::Invalidargument(
            "mysql dsn: missing user".into(),
        ));
    }
    Ok(cfg)
}

export!(Extension);

// ---------------------------------------------------------------------------
// Native unit tests (DSN parsing + type mapping; no network needed).
// ---------------------------------------------------------------------------
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dsn_keyvalue() {
        let c = parse_dsn("host=127.0.0.1 port=3306 user=duck password=duckpw database=ducktest")
            .unwrap();
        assert_eq!(c.host, "127.0.0.1");
        assert_eq!(c.port, 3306);
        assert_eq!(c.user, "duck");
        assert_eq!(c.password, "duckpw");
        assert_eq!(c.database, "ducktest");
    }

    #[test]
    fn dsn_url() {
        let c = parse_dsn("mysql://duck:duckpw@127.0.0.1:3306/ducktest").unwrap();
        assert_eq!(c.host, "127.0.0.1");
        assert_eq!(c.port, 3306);
        assert_eq!(c.user, "duck");
        assert_eq!(c.password, "duckpw");
        assert_eq!(c.database, "ducktest");
    }

    #[test]
    fn dsn_url_defaults() {
        let c = parse_dsn("mysql://duck@host/db").unwrap();
        assert_eq!(c.host, "host");
        assert_eq!(c.port, 3306);
        assert_eq!(c.user, "duck");
        assert_eq!(c.password, "");
        assert_eq!(c.database, "db");
    }

    #[test]
    fn classify_types() {
        assert_eq!(classify_decl("int(11)"), ColType::Int);
        assert_eq!(classify_decl("BIGINT"), ColType::Int);
        assert_eq!(classify_decl("double"), ColType::Float);
        assert_eq!(classify_decl("decimal(10,2)"), ColType::Float);
        assert_eq!(classify_decl("varchar(20)"), ColType::Text);
    }

    #[test]
    fn literal_escaping() {
        assert_eq!(literal(&types::Duckvalue::Int64(5)), "5");
        assert_eq!(literal(&types::Duckvalue::Text("a'b".into())), "'a''b'");
    }
}
