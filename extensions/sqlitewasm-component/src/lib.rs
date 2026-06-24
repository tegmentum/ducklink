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

wit_bindgen::generate!({ path: "./wit", world: "duckdb:extension/duckdb-extension-storage" });

use duckdb::extension::{runtime, storage, types};
use exports::duckdb::extension::{callback_dispatch, guest, storage_dispatch};

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
    fn call_scalar_batch(
        _h: u32,
        _r: Vec<Vec<types::Duckvalue>>,
        _c: types::Invokeinfo,
    ) -> Result<Vec<types::Duckvalue>, types::Duckerror> {
        Err(types::Duckerror::Unsupported("sqlite: no scalar fns".into()))
    }
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

    fn call_aggregate(
        _h: u32,
        _r: types::Rowbatch,
    ) -> Result<types::Duckvalue, types::Duckerror> {
        Err(types::Duckerror::Unsupported("sqlite: no aggs".into()))
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
            Ok(cols
                .into_iter()
                .map(|(name, ty)| types::Columndef {
                    name: name.into(),
                    logical: ty,
                })
                .collect())
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
            let end = (cur.pos + max_rows as usize).min(cur.rows.len());
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
fn run_scan(
    conn: &Connection,
    request: &storage::ScanRequest,
) -> Result<std::vec::Vec<std::vec::Vec<types::Duckvalue>>, types::Duckerror> {
    let cols = table_columns(conn, &request.table)?;

    // Projection: indices into the full column list, in emit order. Empty = all.
    let proj: std::vec::Vec<usize> = if request.projection.is_empty() {
        (0..cols.len()).collect()
    } else {
        request.projection.iter().map(|&i| i as usize).collect()
    };
    for &i in &proj {
        if i >= cols.len() {
            return Err(types::Duckerror::Invalidargument(
                "projection index out of range".into(),
            ));
        }
    }

    let select_list: std::vec::Vec<std::string::String> =
        proj.iter().map(|&i| quote_ident(&cols[i].0)).collect();
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
        if idx >= cols.len() {
            return Err(types::Duckerror::Invalidargument(
                "filter column index out of range".into(),
            ));
        }
        let col = quote_ident(&cols[idx].0);
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
        for (slot, &ci) in proj.iter().enumerate() {
            let v = row.get_ref(slot).map_err(map_sqlite_err)?;
            emit.push(value_to_duck(v, cols[ci].1));
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
    };
    stmt.raw_bind_parameter(idx, val).map_err(map_sqlite_err)
}

/// Map a sqlite value to a duckvalue, coercing toward the projected column's
/// declared logicaltype where it makes sense; NULL always -> Null.
fn value_to_duck(v: ValueRef<'_>, ty: types::Logicaltype) -> types::Duckvalue {
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
    storage::register_storage("sqlite", STORAGE_HANDLE, None)?;
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
        assert_eq!(cols[0].1, types::Logicaltype::Int64);
        assert_eq!(cols[1].0, "b");
        assert_eq!(cols[1].1, types::Logicaltype::Text);
    }

    #[test]
    fn scan_projection_and_filter() {
        let bytes = sample_db_bytes();
        let conn = open_blob(&bytes).unwrap();
        // projection [0] (column `a`); filter: column 0 (`a`) > 1  -> only row 2.
        let req = storage::ScanRequest {
            table: "t".into(),
            projection: vec![0],
            filters: vec![storage::ScanFilter {
                column: 0,
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
}
