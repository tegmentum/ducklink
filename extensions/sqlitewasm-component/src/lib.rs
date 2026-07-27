//! SQLite scanner: the SQLite C library is compiled to wasm INSIDE this
//! component (via rusqlite's `bundled` sqlite3.c) and serves DuckDB two ways:
//!
//!   (a) a `sqlite_scan(db BLOB, table TEXT) -> table` table function that MELTS
//!       a SQLite table into (row_no BIGINT, col TEXT, val TEXT) tuples, and
//!   (b) the storage / pushdown-scan WIT interface (ATTACH a SQLite DB handed
//!       over as a BLOB; columnar projection + filter + limit pushdown).
//!
//! The DB is loaded from BLOB bytes with no filesystem via
//! `sqlite3_deserialize` into an in-memory connection. Nothing panics across
//! the FFI boundary -- every failure maps to a `duckerror`.
use std::cell::RefCell;
use std::collections::HashMap;

use wit_bindgen::rt::string::String;
use wit_bindgen::rt::vec::Vec;

wit_bindgen::generate!({ path: "./wit", world: "duckdb:extension/duckdb-extension-storage-write" });

use duckdb::extension::{runtime, storage, types};
use exports::duckdb::extension::{callback_dispatch, guest, storage_dispatch, storage_write_dispatch};

use rusqlite::types::ValueRef;
use rusqlite::Connection;

/// Opaque callback handle the host passes back to every storage-dispatch call.
const STORAGE_HANDLE: u32 = 1;
/// Opaque handle for the single registered `sqlite_scan` table function.
const TABLE_HANDLE: u32 = 1;

struct Extension;

impl guest::Guest for Extension {
    fn load() -> Result<types::Loadresult, types::Duckerror> {
        register_sqlite_scan()?;
        register_storage_backend()?;
        Ok(types::Loadresult {
            name: "sqlitewasm".into(),
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

// ---------------------------------------------------------------------------
// (a) sqlite_scan table function  (the static-schema MELTED path)
// ---------------------------------------------------------------------------

impl callback_dispatch::Guest for Extension {
    // major-4 columnar dispatch: sqlitewasm is table-only, so the three columnar
    // hot methods are Unsupported stubs; call_table stays hand-written.
    datalink_extcore::columnar_stub!();

    fn call_scalar(
        _h: u32,
        _a: Vec<types::Duckvalue>,
        _c: types::Invokeinfo,
    ) -> Result<types::Duckvalue, types::Duckerror> {
        Err(types::Duckerror::Unsupported("sqlite: no scalar fns".into()))
    }

    fn call_table(
        handle: u32,
        args: Vec<types::Duckvalue>,
    ) -> Result<types::Resultset, types::Duckerror> {
        if handle != TABLE_HANDLE {
            return Err(types::Duckerror::Internal("unknown table handle".into()));
        }
        let mut it = args.into_iter();
        // The db is delivered either as raw BLOB bytes or as a hex STRING. The
        // wasm-DuckDB-core registers table-function parameters as VARCHAR
        // (it does not yet honor declared arg logicaltypes), so the SQL-level
        // entry point passes the database as a hex string which we decode here.
        // The native storage-dispatch path takes real bytes via attach-blob.
        let bytes: std::vec::Vec<u8> = match it.next() {
            Some(types::Duckvalue::Blob(b)) => b.into(),
            Some(types::Duckvalue::Text(s)) => hex_decode(&s).ok_or_else(|| {
                types::Duckerror::Invalidargument(
                    "sqlite_scan: db string must be hex-encoded SQLite bytes".into(),
                )
            })?,
            Some(types::Duckvalue::Null) | None => {
                return Err(types::Duckerror::Invalidargument(
                    "sqlite_scan: db argument is required".into(),
                ))
            }
            _ => {
                return Err(types::Duckerror::Invalidargument(
                    "sqlite_scan: first argument must be a BLOB or hex string".into(),
                ))
            }
        };
        let table = match it.next() {
            Some(types::Duckvalue::Text(s)) => s.to_string(),
            _ => {
                return Err(types::Duckerror::Invalidargument(
                    "sqlite_scan: second argument must be a table name (TEXT)".into(),
                ))
            }
        };

        let conn = open_blob(&bytes)?;
        Ok(scan_melted(&conn, &table)?.into())
    }

    fn call_pragma(
        _h: u32,
        _a: Vec<types::Duckvalue>,
    ) -> Result<Option<types::Duckvalue>, types::Duckerror> {
        Err(types::Duckerror::Unsupported("sqlite: no pragmas".into()))
    }
    fn call_cast(
        _h: u32,
        _v: types::Duckvalue,
    ) -> Result<types::Duckvalue, types::Duckerror> {
        Err(types::Duckerror::Unsupported("sqlite: no casts".into()))
    }
}

/// `SELECT * FROM "<table>"`, melting each row into (row_no, col, val) tuples.
fn scan_melted(
    conn: &Connection,
    table: &str,
) -> Result<std::vec::Vec<std::vec::Vec<types::Duckvalue>>, types::Duckerror> {
    let sql = format!("SELECT * FROM {}", quote_ident(table));
    let mut stmt = conn.prepare(&sql).map_err(map_sqlite_err)?;
    let ncols = stmt.column_count();
    let names: std::vec::Vec<std::string::String> = (0..ncols)
        .map(|i| stmt.column_name(i).unwrap_or("?").to_string())
        .collect();

    let mut out: std::vec::Vec<std::vec::Vec<types::Duckvalue>> = std::vec::Vec::new();
    let mut rows = stmt.query([]).map_err(map_sqlite_err)?;
    let mut row_no: i64 = 0;
    while let Some(row) = rows.next().map_err(map_sqlite_err)? {
        for c in 0..ncols {
            let v = row.get_ref(c).map_err(map_sqlite_err)?;
            let val = match v {
                ValueRef::Null => types::Duckvalue::Null,
                other => value_as_text(other),
            };
            out.push(vec![
                types::Duckvalue::Int64(row_no),
                types::Duckvalue::Text(names[c].clone().into()),
                val,
            ]);
        }
        row_no += 1;
    }
    Ok(out)
}

/// Render any non-null sqlite value as TEXT (for the melted path's `val` slot).
/// Decode an ASCII hex string into bytes (even length, [0-9a-fA-F]); None on
/// any invalid character or odd length. No dependency, never panics.
fn hex_decode(s: &str) -> Option<std::vec::Vec<u8>> {
    let s = s.trim();
    if s.len() % 2 != 0 {
        return None;
    }
    let nib = |c: u8| -> Option<u8> {
        match c {
            b'0'..=b'9' => Some(c - b'0'),
            b'a'..=b'f' => Some(c - b'a' + 10),
            b'A'..=b'F' => Some(c - b'A' + 10),
            _ => None,
        }
    };
    let b = s.as_bytes();
    let mut out = std::vec::Vec::with_capacity(b.len() / 2);
    let mut i = 0;
    while i < b.len() {
        out.push((nib(b[i])? << 4) | nib(b[i + 1])?);
        i += 2;
    }
    Some(out)
}

fn value_as_text(v: ValueRef<'_>) -> types::Duckvalue {
    match v {
        ValueRef::Null => types::Duckvalue::Null,
        ValueRef::Integer(i) => types::Duckvalue::Text(i.to_string().into()),
        ValueRef::Real(f) => types::Duckvalue::Text(f.to_string().into()),
        ValueRef::Text(t) => {
            types::Duckvalue::Text(std::string::String::from_utf8_lossy(t).into_owned().into())
        }
        ValueRef::Blob(b) => {
            // Hex-encode blobs so the melted TEXT slot stays printable.
            let mut s = std::string::String::with_capacity(b.len() * 2);
            for byte in b {
                s.push_str(&format!("{byte:02x}"));
            }
            types::Duckvalue::Text(s.into())
        }
    }
}

// ---------------------------------------------------------------------------
// (c) storage-dispatch: columnar projection + filter + limit pushdown
// ---------------------------------------------------------------------------

/// Per-component storage state, kept thread-local (the component is
/// single-threaded under wasip2).
struct Cursor {
    rows: std::vec::Vec<std::vec::Vec<types::Duckvalue>>,
    pos: usize,
}

thread_local! {
    /// Staged blobs keyed by ATTACH dsn, awaiting a storage-attach.
    static STAGED: RefCell<HashMap<std::string::String, std::vec::Vec<u8>>> =
        RefCell::new(HashMap::new());
    /// Open catalogs keyed by catalog-id.
    static CATALOGS: RefCell<HashMap<u32, Connection>> = RefCell::new(HashMap::new());
    /// Materialized scan cursors keyed by scan-id.
    static SCANS: RefCell<HashMap<u32, Cursor>> = RefCell::new(HashMap::new());
    static NEXT_CATALOG: RefCell<u32> = const { RefCell::new(1) };
    static NEXT_SCAN: RefCell<u32> = const { RefCell::new(1) };
}

impl storage_dispatch::Guest for Extension {
    fn attach_blob(
        handle: u32,
        dsn: String,
        bytes: Vec<u8>,
    ) -> Result<(), types::Duckerror> {
        check_handle(handle)?;
        STAGED.with(|s| {
            s.borrow_mut().insert(dsn.to_string(), bytes.into());
        });
        Ok(())
    }

    fn storage_attach(
        handle: u32,
        dsn: String,
        _options: Vec<(String, String)>,
    ) -> Result<u32, types::Duckerror> {
        check_handle(handle)?;
        let bytes = STAGED
            .with(|s| s.borrow_mut().remove(&dsn.to_string()))
            .ok_or_else(|| {
                types::Duckerror::Invalidstate(format!("no staged blob for dsn '{dsn}'"))
            })?;
        let conn = open_blob(&bytes)?;
        let id = NEXT_CATALOG.with(|n| {
            let mut n = n.borrow_mut();
            let id = *n;
            *n += 1;
            id
        });
        CATALOGS.with(|c| c.borrow_mut().insert(id, conn));
        Ok(id)
    }

    fn storage_list_tables(
        handle: u32,
        catalog: u32,
    ) -> Result<Vec<String>, types::Duckerror> {
        check_handle(handle)?;
        CATALOGS.with(|c| {
            let c = c.borrow();
            let conn = c
                .get(&catalog)
                .ok_or_else(|| types::Duckerror::Invalidstate("unknown catalog".into()))?;
            let mut stmt = conn
                .prepare("SELECT name FROM sqlite_master WHERE type='table' ORDER BY name")
                .map_err(map_sqlite_err)?;
            let names = stmt
                .query_map([], |row| row.get::<_, std::string::String>(0))
                .map_err(map_sqlite_err)?;
            let mut out: Vec<String> = Vec::new();
            for n in names {
                out.push(n.map_err(map_sqlite_err)?.into());
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
        CATALOGS.with(|c| {
            let c = c.borrow();
            let conn = c
                .get(&catalog)
                .ok_or_else(|| types::Duckerror::Invalidstate("unknown catalog".into()))?;
            let cols = table_columns(conn, &table)?;
            // ADR Amendment A5: prepend a synthetic `rowid` (Int64) column at
            // index 0 so the host's `at5_locate_rowid_column` finds it and
            // the write-path pre-scan can extract SQLite's native ROWID from
            // the emitted rows (see docs/at5-rowid-mechanism.md).
            let mut out: Vec<types::Columndef> = Vec::with_capacity(cols.len() + 1);
            out.push(types::Columndef {
                name: "rowid".into(),
                logical: types::Logicaltype::Int64,
            });
            for (name, ty) in cols {
                out.push(types::Columndef {
                    name: name.into(),
                    logical: ty,
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
        let rows = CATALOGS.with(|c| {
            let c = c.borrow();
            let conn = c
                .get(&catalog)
                .ok_or_else(|| types::Duckerror::Invalidstate("unknown catalog".into()))?;
            run_scan(conn, &request)
        })?;
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
            // wasm32 target: `usize` is 32-bit, so `cur.pos + max_rows` (with
            // e.g. `max_rows = u32::MAX`, which the host uses to mean "drain
            // everything") overflows and wraps modulo 2^32. In release the
            // wrap is silent — the wrapped `end` compares less than `cur.pos`
            // and `cur.rows[cur.pos..end]` panics with
            // "slice index starts at N but ends at M". `saturating_add`
            // (paired with `.min(rows.len())` for the ceiling) matches the
            // documented "give me up to `max_rows`" contract cleanly, and the
            // explicit `pos >= rows.len()` short-circuit turns end-of-cursor
            // into an empty batch instead of a zero-width slice, so the
            // host's drain loop terminates on the first EOF call.
            if cur.pos >= cur.rows.len() {
                return Ok(std::vec::Vec::<std::vec::Vec<types::Duckvalue>>::new().into());
            }
            let end = cur
                .pos
                .saturating_add(max_rows as usize)
                .min(cur.rows.len());
            let batch: std::vec::Vec<std::vec::Vec<types::Duckvalue>> =
                cur.rows[cur.pos..end].to_vec();
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
        CATALOGS.with(|c| c.borrow_mut().remove(&catalog));
        Ok(true)
    }

    /// AT5 write-back: serialize the in-memory SQLite database back to raw
    /// database-file bytes via `sqlite3_serialize`. The host writes the returned
    /// blob back to the ATTACH DSN path after each successful INSERT/UPDATE/
    /// DELETE so the mutation persists on disk (the in-memory copy is opened
    /// via `sqlite3_deserialize` in `open_blob`; without this it never leaves
    /// the wasm heap).
    fn serialize(handle: u32, catalog: u32) -> Result<Vec<u8>, types::Duckerror> {
        check_handle(handle)?;
        CATALOGS.with(|c| {
            let c = c.borrow();
            let conn = c.get(&catalog).ok_or_else(|| {
                types::Duckerror::Invalidstate("unknown catalog".into())
            })?;
            // SAFETY: sqlite owns the returned buffer; we memcpy into a Rust
            // Vec and then `sqlite3_free` the sqlite side. A null return means
            // OOM or a serialize failure; both surface as a duckerror. `main`
            // is the same schema label sqlite3_deserialize used in open_blob.
            unsafe {
                let db = conn.handle();
                let mut len: i64 = 0;
                let p = libsqlite3_sys::sqlite3_serialize(
                    db,
                    b"main\0".as_ptr() as *const _,
                    &mut len as *mut i64,
                    0,
                );
                if p.is_null() {
                    return Err(types::Duckerror::Io(
                        "sqlite3_serialize returned null".into(),
                    ));
                }
                let bytes = std::slice::from_raw_parts(p as *const u8, len as usize)
                    .to_vec();
                libsqlite3_sys::sqlite3_free(p as *mut _);
                Ok(bytes.into())
            }
        })
    }
}

// ---------------------------------------------------------------------------
// (d) storage-write-dispatch: transactions + DDL + DML (ADR Amendment A5)
// ---------------------------------------------------------------------------

thread_local! {
    /// Open write-transactions keyed by txn-id. Each entry stores the
    /// catalog-id the transaction was begun on, so the write-side calls
    /// (insert/update/delete/create-table) can find the right sqlite
    /// Connection under the CATALOGS map without the host having to hand
    /// the catalog back on every call.
    static TXNS: RefCell<HashMap<u32, u32>> = RefCell::new(HashMap::new());
    static NEXT_TXN: RefCell<u32> = const { RefCell::new(1) };
}

impl storage_write_dispatch::Guest for Extension {
    fn begin_transaction(handle: u32, catalog: u32) -> Result<u32, types::Duckerror> {
        check_handle(handle)?;
        CATALOGS.with(|c| {
            let c = c.borrow();
            let conn = c.get(&catalog).ok_or_else(|| {
                types::Duckerror::Invalidstate("unknown catalog".into())
            })?;
            conn.execute_batch("BEGIN").map_err(map_sqlite_err)
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
        with_catalog_conn(catalog, |conn| {
            conn.execute_batch("COMMIT").map_err(map_sqlite_err)
        })
    }

    fn rollback_transaction(handle: u32, txn: u32) -> Result<(), types::Duckerror> {
        check_handle(handle)?;
        let catalog = take_txn(txn)?;
        with_catalog_conn(catalog, |conn| {
            conn.execute_batch("ROLLBACK").map_err(map_sqlite_err)
        })
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
            .map(|c| format!("{} {}", quote_ident(&c.name), logical_to_sqlite(&c.logical)))
            .collect();
        let sql = format!(
            "CREATE TABLE {} ({})",
            quote_ident(&table),
            col_defs.join(", ")
        );
        with_catalog_conn(catalog, |conn| {
            conn.execute_batch(&sql).map_err(map_sqlite_err)
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
        // Rows arrive with the extension's advertised column shape --
        // [rowid, col1, col2, ...] per ADR Amendment A5. Strip the leading
        // rowid slot when it is present: an INSERT lets SQLite mint the
        // rowid, we do not overwrite it. If the caller emits a bare N-col
        // shape (== underlying_cols) we pass through unchanged.
        let n_underlying = with_catalog_conn_result(catalog, |conn| {
            Ok(table_columns(conn, &table)?.len())
        })?;
        let width = rows[0].len();
        let strip_rowid = width == n_underlying + 1;
        if !strip_rowid && width != n_underlying {
            return Err(types::Duckerror::Invalidargument(format!(
                "insert-rows into '{table}': expected {n_underlying} or \
                 {} values per row, got {width}",
                n_underlying + 1
            )));
        }
        let n = with_catalog_conn_result(catalog, |conn| {
            let cols = table_columns(conn, &table)?;
            let col_list: std::vec::Vec<std::string::String> =
                cols.iter().map(|(n, _)| quote_ident(n)).collect();
            let placeholders: std::vec::Vec<std::string::String> =
                (0..cols.len()).map(|_| "?".to_string()).collect();
            let sql = format!(
                "INSERT INTO {} ({}) VALUES ({})",
                quote_ident(&table),
                col_list.join(", "),
                placeholders.join(", ")
            );
            let mut inserted: u64 = 0;
            for row in &rows {
                let payload: &[types::Duckvalue] = if strip_rowid {
                    &row[1..]
                } else {
                    &row[..]
                };
                let mut stmt = conn.prepare(&sql).map_err(map_sqlite_err)?;
                for (i, v) in payload.iter().enumerate() {
                    bind_value(&mut stmt, i + 1, v)?;
                }
                stmt.raw_execute().map_err(map_sqlite_err)?;
                inserted += 1;
            }
            Ok(inserted)
        })?;
        Ok(n)
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
        with_catalog_conn_result(catalog, |conn| {
            let sql = format!("DELETE FROM {} WHERE rowid = ?", quote_ident(&table));
            let mut deleted: u64 = 0;
            for rid in &rowids {
                let mut stmt = conn.prepare(&sql).map_err(map_sqlite_err)?;
                stmt.raw_bind_parameter(1, *rid).map_err(map_sqlite_err)?;
                stmt.raw_execute().map_err(map_sqlite_err)?;
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
        let n_underlying = with_catalog_conn_result(catalog, |conn| {
            Ok(table_columns(conn, &table)?.len())
        })?;
        let width = rows[0].len();
        // The intercept_write path passes ROWS at the extension's advertised
        // width (rowid + underlying cols). The rowid slot at index 0 is
        // ignored -- WHERE rowid = ? routes the write, not the row payload.
        let strip_rowid = width == n_underlying + 1;
        if !strip_rowid && width != n_underlying {
            return Err(types::Duckerror::Invalidargument(format!(
                "update-rows for '{table}': expected {n_underlying} or \
                 {} values per row, got {width}",
                n_underlying + 1
            )));
        }
        with_catalog_conn_result(catalog, |conn| {
            let cols = table_columns(conn, &table)?;
            let set_list: std::vec::Vec<std::string::String> = cols
                .iter()
                .map(|(n, _)| format!("{} = ?", quote_ident(n)))
                .collect();
            let sql = format!(
                "UPDATE {} SET {} WHERE rowid = ?",
                quote_ident(&table),
                set_list.join(", "),
            );
            let mut updated: u64 = 0;
            for (rid, row) in rowids.iter().zip(rows.iter()) {
                let payload: &[types::Duckvalue] = if strip_rowid {
                    &row[1..]
                } else {
                    &row[..]
                };
                let mut stmt = conn.prepare(&sql).map_err(map_sqlite_err)?;
                for (i, v) in payload.iter().enumerate() {
                    bind_value(&mut stmt, i + 1, v)?;
                }
                stmt.raw_bind_parameter(payload.len() + 1, *rid)
                    .map_err(map_sqlite_err)?;
                stmt.raw_execute().map_err(map_sqlite_err)?;
                updated += 1;
            }
            Ok(updated)
        })
    }
}

/// Fetch the catalog a transaction was begun on; error if the txn is unknown
/// (double-commit / stray rollback). Leaves the entry in place -- the caller
/// takes it out via `take_txn` on commit/rollback.
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

fn with_catalog_conn<F, T>(catalog: u32, f: F) -> Result<T, types::Duckerror>
where
    F: FnOnce(&Connection) -> Result<T, types::Duckerror>,
{
    CATALOGS.with(|c| {
        let c = c.borrow();
        let conn = c
            .get(&catalog)
            .ok_or_else(|| types::Duckerror::Invalidstate("unknown catalog".into()))?;
        f(conn)
    })
}

fn with_catalog_conn_result<F, T>(catalog: u32, f: F) -> Result<T, types::Duckerror>
where
    F: FnOnce(&Connection) -> Result<T, types::Duckerror>,
{
    with_catalog_conn(catalog, f)
}

/// Map an extension logicaltype to a SQLite column type name for CREATE TABLE.
/// SQLite's declared type is advisory (type affinity) so we lean on a small
/// canonical set -- INTEGER / REAL / TEXT / BLOB / BOOLEAN.
fn logical_to_sqlite(ty: &types::Logicaltype) -> &'static str {
    match ty {
        types::Logicaltype::Boolean => "INTEGER",
        types::Logicaltype::Int8
        | types::Logicaltype::Int16
        | types::Logicaltype::Int32
        | types::Logicaltype::Int64
        | types::Logicaltype::Uint8
        | types::Logicaltype::Uint16
        | types::Logicaltype::Uint32
        | types::Logicaltype::Uint64 => "INTEGER",
        types::Logicaltype::Float32 | types::Logicaltype::Float64 => "REAL",
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

/// `PRAGMA table_info` -> ordered (name, logicaltype) pairs.
fn table_columns(
    conn: &Connection,
    table: &str,
) -> Result<std::vec::Vec<(std::string::String, types::Logicaltype)>, types::Duckerror> {
    let sql = format!("PRAGMA table_info({})", quote_ident(table));
    let mut stmt = conn.prepare(&sql).map_err(map_sqlite_err)?;
    let rows = stmt
        .query_map([], |row| {
            let name: std::string::String = row.get(1)?;
            let decl: std::string::String = row.get(2)?;
            Ok((name, decl))
        })
        .map_err(map_sqlite_err)?;
    let mut out = std::vec::Vec::new();
    for r in rows {
        let (name, decl) = r.map_err(map_sqlite_err)?;
        out.push((name, map_decl_type(&decl)));
    }
    if out.is_empty() {
        return Err(types::Duckerror::Invalidargument(format!(
            "table '{table}' not found or has no columns"
        )));
    }
    Ok(out)
}

/// Map a declared SQLite column type to a DuckDB logicaltype.
fn map_decl_type(decl: &str) -> types::Logicaltype {
    let d = decl.trim().to_ascii_uppercase();
    match d.as_str() {
        "INTEGER" => types::Logicaltype::Int64,
        "REAL" => types::Logicaltype::Float64,
        "TEXT" => types::Logicaltype::Text,
        "BLOB" => types::Logicaltype::Blob,
        _ => types::Logicaltype::Text,
    }
}

/// Build + execute the pushdown SQL for a scan-request, materializing all rows.
///
/// Host-side column indices (`request.projection` and `request.filters[*].column`)
/// are 1-based into the underlying SQLite table's columns: index 0 references
/// the synthetic `rowid` column the extension advertises in
/// `storage_table_columns` (ADR Amendment A5 / docs/at5-rowid-mechanism.md).
/// index >= 1 references `cols[i-1]`.
fn run_scan(
    conn: &Connection,
    request: &storage::ScanRequest,
) -> Result<std::vec::Vec<std::vec::Vec<types::Duckvalue>>, types::Duckerror> {
    let cols = table_columns(conn, &request.table)?;
    // The synthetic column list the host sees: [rowid, cols[0], cols[1], ...].
    // `n_host_cols` includes the rowid slot at position 0.
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

    // Map a synthetic-list index to a bare SQL expression: `rowid` for index 0,
    // otherwise the quoted underlying column name.
    let sql_col = |host_idx: usize| -> std::string::String {
        if host_idx == 0 {
            "rowid".to_string()
        } else {
            quote_ident(&cols[host_idx - 1].0)
        }
    };

    let select_list: std::vec::Vec<std::string::String> =
        proj.iter().map(|&i| sql_col(i)).collect();
    let mut sql = format!(
        "SELECT {} FROM {}",
        select_list.join(", "),
        quote_ident(&request.table)
    );

    // WHERE: AND-join the filters. Bound values are collected in order.
    let mut binds: std::vec::Vec<&types::Duckvalue> = std::vec::Vec::new();
    let mut conds: std::vec::Vec<std::string::String> = std::vec::Vec::new();
    for f in &request.filters {
        let idx = f.column as usize;
        if idx >= n_host_cols {
            return Err(types::Duckerror::Invalidargument(
                "filter column index out of range".into(),
            ));
        }
        let col = sql_col(idx);
        match f.op {
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
                conds.push(format!("{col} {sym} ?"));
                binds.push(&f.value);
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

    let mut stmt = conn.prepare(&sql).map_err(map_sqlite_err)?;
    // Bind parameters (1-indexed in sqlite).
    for (i, v) in binds.iter().enumerate() {
        bind_value(&mut stmt, i + 1, v)?;
    }

    let mut rows = stmt.raw_query();
    let mut out: std::vec::Vec<std::vec::Vec<types::Duckvalue>> = std::vec::Vec::new();
    while let Some(row) = rows.next().map_err(map_sqlite_err)? {
        let mut emit: std::vec::Vec<types::Duckvalue> = std::vec::Vec::with_capacity(proj.len());
        for (slot, &host_idx) in proj.iter().enumerate() {
            let v = row.get_ref(slot).map_err(map_sqlite_err)?;
            let rowid_logical = types::Logicaltype::Int64;
            let logical = if host_idx == 0 {
                &rowid_logical
            } else {
                &cols[host_idx - 1].1
            };
            emit.push(value_to_duck(v, logical));
        }
        out.push(emit);
    }
    Ok(out)
}

/// Bind a duckvalue as a sqlite statement parameter.
fn bind_value(
    stmt: &mut rusqlite::Statement<'_>,
    idx: usize,
    v: &types::Duckvalue,
) -> Result<(), types::Duckerror> {
    use rusqlite::types::Value;
    let val = match v {
        types::Duckvalue::Null => Value::Null,
        types::Duckvalue::Boolean(b) => Value::Integer(if *b { 1 } else { 0 }),
        types::Duckvalue::Int64(i) => Value::Integer(*i),
        types::Duckvalue::Uint64(u) => Value::Integer(*u as i64),
        types::Duckvalue::Float64(f) => Value::Real(*f),
        types::Duckvalue::Text(s) => Value::Text(s.to_string()),
        types::Duckvalue::Blob(b) => Value::Blob(b.to_vec()),
        types::Duckvalue::Int8(i) => Value::Integer(*i as i64),
        types::Duckvalue::Int16(i) => Value::Integer(*i as i64),
        types::Duckvalue::Int32(i) => Value::Integer(*i as i64),
        types::Duckvalue::Uint8(u) => Value::Integer(*u as i64),
        types::Duckvalue::Uint16(u) => Value::Integer(*u as i64),
        types::Duckvalue::Uint32(u) => Value::Integer(*u as i64),
        types::Duckvalue::Float32(f) => Value::Real(*f as f64),
        types::Duckvalue::Date(d) => Value::Integer(*d as i64),
        types::Duckvalue::Time(t) => Value::Integer(*t),
        types::Duckvalue::Timestamp(t) => Value::Integer(*t),
        types::Duckvalue::Timestamptz(t) => Value::Integer(*t),
        // DECIMAL/INTERVAL/UUID have no native SQLite scalar; bind a faithful
        // text rendering (UUID hex, the decimal's raw int128, interval micros).
        types::Duckvalue::Decimal(d) => {
            Value::Integer((((d.upper as u128) << 64) | d.lower as u128) as i64)
        }
        types::Duckvalue::Interval(iv) => Value::Integer(iv.micros),
        types::Duckvalue::Uuid(u) => {
            Value::Text(format!("{:016x}{:016x}", u.hi, u.lo))
        }
        // major-5 T2-1: HUGEINT / UHUGEINT 128-bit integers. SQLite has no
        // native 128-bit scalar, so we bind the reassembled value as text (the
        // decimal representation is faithful and round-trippable).
        types::Duckvalue::Hugeint(h) => {
            let v = ((h.upper as i128) << 64) | (h.lower as i128);
            Value::Text(v.to_string())
        }
        types::Duckvalue::Uhugeint(u) => {
            let v = ((u.upper as u128) << 64) | (u.lower as u128);
            Value::Text(v.to_string())
        }
        // ESCAPE-HATCH: a nested value -- bind its JSON text into sqlite.
        types::Duckvalue::Complex(c) => Value::Text(c.json.to_string()),
    };
    stmt.raw_bind_parameter(idx, val).map_err(map_sqlite_err)
}

/// Map a sqlite value to a duckvalue, coercing toward the projected column's
/// declared logicaltype where it makes sense; NULL always -> Null.
fn value_to_duck(v: ValueRef<'_>, ty: &types::Logicaltype) -> types::Duckvalue {
    match v {
        ValueRef::Null => types::Duckvalue::Null,
        ValueRef::Integer(i) => match ty {
            types::Logicaltype::Float64 => types::Duckvalue::Float64(i as f64),
            types::Logicaltype::Text => types::Duckvalue::Text(i.to_string().into()),
            types::Logicaltype::Boolean => types::Duckvalue::Boolean(i != 0),
            _ => types::Duckvalue::Int64(i),
        },
        ValueRef::Real(f) => match ty {
            types::Logicaltype::Int64 => types::Duckvalue::Int64(f as i64),
            types::Logicaltype::Text => types::Duckvalue::Text(f.to_string().into()),
            _ => types::Duckvalue::Float64(f),
        },
        ValueRef::Text(t) => types::Duckvalue::Text(
            std::string::String::from_utf8_lossy(t).into_owned().into(),
        ),
        ValueRef::Blob(b) => match ty {
            types::Logicaltype::Text => types::Duckvalue::Text(
                std::string::String::from_utf8_lossy(b).into_owned().into(),
            ),
            _ => types::Duckvalue::Blob(b.to_vec().into()),
        },
    }
}

// ---------------------------------------------------------------------------
// DESERIALIZE: load a SQLite DB from BLOB bytes with no filesystem.
// ---------------------------------------------------------------------------

/// Open an in-memory SQLite connection seeded from raw DB-file `bytes` via
/// `sqlite3_deserialize`. Never panics; FFI failures map to a duckerror.
fn open_blob(bytes: &[u8]) -> Result<Connection, types::Duckerror> {
    let conn = Connection::open_in_memory()
        .map_err(|e| types::Duckerror::Internal(format!("open_in_memory: {e}")))?;
    let len = bytes.len();
    if len == 0 {
        return Err(types::Duckerror::Invalidargument(
            "empty SQLite database blob".into(),
        ));
    }

    // SAFETY: we hand sqlite an sqlite-owned copy of the bytes and let it own /
    // free it (FREEONCLOSE). The Connection outlives the deserialize call and is
    // returned to the caller, who keeps it alive for the catalog's lifetime.
    let rc = unsafe {
        let db = conn.handle();
        let p = libsqlite3_sys::sqlite3_malloc(len as i32) as *mut u8;
        if p.is_null() {
            return Err(types::Duckerror::Io("sqlite3_malloc returned null".into()));
        }
        std::ptr::copy_nonoverlapping(bytes.as_ptr(), p, len);
        libsqlite3_sys::sqlite3_deserialize(
            db,
            b"main\0".as_ptr() as *const _,
            p,
            len as i64,
            len as i64,
            (libsqlite3_sys::SQLITE_DESERIALIZE_FREEONCLOSE
                | libsqlite3_sys::SQLITE_DESERIALIZE_RESIZEABLE) as u32,
        )
    };
    if rc != libsqlite3_sys::SQLITE_OK {
        return Err(types::Duckerror::Io(format!(
            "sqlite3_deserialize failed (rc={rc})"
        )));
    }
    Ok(conn)
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Quote a SQL identifier by doubling embedded double-quotes.
fn quote_ident(name: &str) -> std::string::String {
    format!("\"{}\"", name.replace('"', "\"\""))
}

fn map_sqlite_err(e: rusqlite::Error) -> types::Duckerror {
    types::Duckerror::Io(format!("sqlite: {e}"))
}

// ---------------------------------------------------------------------------
// Registration
// ---------------------------------------------------------------------------

fn register_sqlite_scan() -> Result<(), types::Duckerror> {
    let cap = runtime::get_capability(types::Capabilitykind::Table)
        .ok_or_else(|| types::Duckerror::Internal("no table capability".into()))?;
    let reg = match cap {
        runtime::Capability::Table(r) => r,
        _ => return Err(types::Duckerror::Internal("bad capability".into())),
    };
    let args = vec![
        runtime::Funcarg {
            name: Some("db".into()),
            // Declared Text: the core registers table params as VARCHAR, so the
            // db arrives as a hex string (decoded in call_table). Real bytes go
            // through the storage-dispatch attach-blob path.
            logical: types::Logicaltype::Text,
        },
        runtime::Funcarg {
            name: Some("table".into()),
            logical: types::Logicaltype::Text,
        },
    ];
    let columns = vec![
        types::Columndef {
            name: "row_no".into(),
            logical: types::Logicaltype::Int64,
        },
        types::Columndef {
            name: "col".into(),
            logical: types::Logicaltype::Text,
        },
        types::Columndef {
            name: "val".into(),
            logical: types::Logicaltype::Text,
        },
    ];
    let opts = runtime::Extopts {
        description: Some(
            "Read a SQLite database handed in as a BLOB, melting <table> into \
             (row_no, col, val) rows"
                .into(),
        ),
        tags: vec!["sqlite".into(), "scanner".into()],
    };
    reg.register(
        "sqlite_blob_scan",
        &args,
        &columns,
        runtime::TableCallback::new(TABLE_HANDLE),
        Some(&opts),
    )?;
    Ok(())
}

fn register_storage_backend() -> Result<(), types::Duckerror> {
    // M2d: the core's StorageExtension is keyed by the ATTACH TYPE name
    // "sqlitewasm"; register under that name so the host's storage-backend lookup
    // matches directly (the host keeps a single-backend fallback regardless).
    storage::register_storage("sqlitewasm", STORAGE_HANDLE, None)?;
    Ok(())
}

export!(Extension);

// ---------------------------------------------------------------------------
// Native unit tests (run with `cargo test` on the host; rusqlite bundled
// builds for the host too, so the storage logic is provable in-sandbox).
// ---------------------------------------------------------------------------
#[cfg(test)]
mod tests {
    use super::*;

    /// Build a deterministic SQLite DB in memory, serialize it to bytes via the
    /// raw `sqlite3_serialize` FFI (the `serialize` rusqlite feature is off).
    fn sample_db_bytes() -> std::vec::Vec<u8> {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE t(a INTEGER, b TEXT);
             INSERT INTO t VALUES (1, 'x'), (2, 'y');
             CREATE TABLE other(z REAL);",
        )
        .unwrap();
        unsafe {
            let db = conn.handle();
            let mut len: i64 = 0;
            let p = libsqlite3_sys::sqlite3_serialize(
                db,
                b"main\0".as_ptr() as *const _,
                &mut len as *mut i64,
                0,
            );
            assert!(!p.is_null(), "sqlite3_serialize returned null");
            let out = std::slice::from_raw_parts(p as *const u8, len as usize).to_vec();
            libsqlite3_sys::sqlite3_free(p as *mut _);
            out
        }
    }

    #[test]
    fn open_blob_roundtrips() {
        let bytes = sample_db_bytes();
        let conn = open_blob(&bytes).expect("open_blob");
        let n: i64 = conn
            .query_row("SELECT count(*) FROM t", [], |r| r.get(0))
            .unwrap();
        assert_eq!(n, 2);
    }

    #[test]
    fn lists_tables_sorted() {
        let bytes = sample_db_bytes();
        let conn = open_blob(&bytes).unwrap();
        let mut stmt = conn
            .prepare("SELECT name FROM sqlite_master WHERE type='table' ORDER BY name")
            .unwrap();
        let names: std::vec::Vec<std::string::String> = stmt
            .query_map([], |r| r.get(0))
            .unwrap()
            .map(|r| r.unwrap())
            .collect();
        assert_eq!(names, vec!["other".to_string(), "t".to_string()]);
    }

    #[test]
    fn table_columns_map_types() {
        let bytes = sample_db_bytes();
        let conn = open_blob(&bytes).unwrap();
        let cols = table_columns(&conn, "t").unwrap();
        assert_eq!(cols.len(), 2);
        assert_eq!(cols[0].0, "a");
        assert!(matches!(cols[0].1, types::Logicaltype::Int64));
        assert_eq!(cols[1].0, "b");
        assert!(matches!(cols[1].1, types::Logicaltype::Text));
    }

    #[test]
    fn scan_projection_and_filter() {
        let bytes = sample_db_bytes();
        let conn = open_blob(&bytes).unwrap();
        // Host-side column indices are 1-based over the sqlite columns; index 0
        // is the synthetic `rowid` column added per ADR Amendment A5. So the
        // projection [1] asks for the sqlite column `a`; filter column 1
        // ("a") > 1 -> only row 2.
        let req = storage::ScanRequest {
            table: "t".into(),
            projection: vec![1],
            filters: vec![storage::ScanFilter {
                column: 1,
                op: storage::CompareOp::Gt,
                value: types::Duckvalue::Int64(1),
            }],
            limit: None,
        };
        let rows = run_scan(&conn, &req).unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].len(), 1);
        match &rows[0][0] {
            types::Duckvalue::Int64(v) => assert_eq!(*v, 2),
            other => panic!("expected Int64(2), got {other:?}"),
        }
    }

    #[test]
    fn scan_returns_rowid_at_index_zero() {
        // Empty projection = all columns; the emitted rows must be
        // [rowid, a, b] with rowid at index 0 (ADR Amendment A5).
        let bytes = sample_db_bytes();
        let conn = open_blob(&bytes).unwrap();
        let req = storage::ScanRequest {
            table: "t".into(),
            projection: Vec::new(),
            filters: Vec::new(),
            limit: None,
        };
        let rows = run_scan(&conn, &req).unwrap();
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].len(), 3, "cols: [rowid, a, b]");
        // rowid is index 0; a is index 1; b is index 2. First inserted row
        // has SQLite rowid = 1 by default.
        match &rows[0][0] {
            types::Duckvalue::Int64(v) => assert_eq!(*v, 1),
            other => panic!("expected rowid Int64(1), got {other:?}"),
        }
        match &rows[1][0] {
            types::Duckvalue::Int64(v) => assert_eq!(*v, 2),
            other => panic!("expected rowid Int64(2), got {other:?}"),
        }
    }

    #[test]
    fn melted_scan_shape() {
        let bytes = sample_db_bytes();
        let conn = open_blob(&bytes).unwrap();
        let rows = scan_melted(&conn, "t").unwrap();
        // 2 data rows * 2 columns = 4 melted tuples.
        assert_eq!(rows.len(), 4);
        // first tuple: row_no=0, col="a", val="1"
        match (&rows[0][0], &rows[0][1], &rows[0][2]) {
            (
                types::Duckvalue::Int64(rn),
                types::Duckvalue::Text(col),
                types::Duckvalue::Text(val),
            ) => {
                assert_eq!(*rn, 0);
                assert_eq!(col.as_str(), "a");
                assert_eq!(val.as_str(), "1");
            }
            other => panic!("unexpected melted tuple: {other:?}"),
        }
    }

    // ---- fuzz regressions (cargo-fuzz; fuzz/fuzz_targets/hex_decode.rs) ------
    // hex_decode parses an untrusted hex VARCHAR into the SQLite DB bytes. A
    // ~18M-execution fuzz campaign found NO panic; these pin the never-panic
    // contract for the boundary cases the fuzzer explored.
    #[test]
    fn hex_decode_is_total() {
        assert_eq!(hex_decode("deadbeef"), Some(vec![0xde, 0xad, 0xbe, 0xef]));
        assert_eq!(hex_decode("DEADBEEF"), Some(vec![0xde, 0xad, 0xbe, 0xef]));
        assert_eq!(hex_decode(""), Some(vec![])); // empty is valid (even length 0)
        assert_eq!(hex_decode("  0a0b  "), Some(vec![0x0a, 0x0b])); // trimmed
        assert_eq!(hex_decode("abc"), None); // odd length
        assert_eq!(hex_decode("zz"), None); // non-hex
        assert_eq!(hex_decode("0g"), None); // partial non-hex
        // Multi-byte UTF-8 / control chars must not panic (they're not hex).
        assert_eq!(hex_decode("é"), None);
        assert_eq!(hex_decode("\0\0"), None);
    }
}
