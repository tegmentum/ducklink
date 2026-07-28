//! Bug 4b: SQLite storage backend against sqlite-lib.
//!
//! SQLite itself no longer lives inside this component. sqlite-lib.wasm (from
//! tegmentum/sqlite-wasm) is composed in at build time via `wac plug`, and
//! every SQL statement runs through `sqlite:extension/spi`. Two surfaces are
//! offered:
//!
//!   (a) `sqlite_blob_scan(db TEXT, table TEXT) -> table` — melts a foreign
//!       SQLite table into (row_no BIGINT, col TEXT, val TEXT) tuples. The
//!       DB arrives as a hex string, is decoded, and loaded via
//!       `spi::deserialize_db` on a fresh in-memory SPI connection.
//!
//!   (b) The storage / storage-write-dispatch WIT surface (ATTACH). Opens the
//!       DSN through `spi::open_db(path)`, which on non-`:memory:` paths goes
//!       through wasivfs -> wasi:filesystem -> the ducklink host's preopens
//!       (cwd + DUCKLINK_SQLITEWASM_DATADIR — see
//!       `crates/ducklink-host/src/lib.rs::attach_sqlitewasm_preopens`).
//!       Writes persist DIRECTLY (`writes-persist-directly = true`) — no host
//!       serialize/deserialize dance, no host-side rusqlite replay path.
//!
//! v1 single-connection limit: sqlite-lib's SPI exposes exactly one shared
//! connection, so only one ATTACH is supported at a time. A second ATTACH
//! reports `Duckerror::Unsupported` telling the user to DETACH first.
//! Multi-attach lifts once sqlite-wasm's high-level connection resource
//! grows `deserialize` (currently missing).
use std::cell::RefCell;
use std::collections::HashMap;

use wit_bindgen::rt::string::String;
use wit_bindgen::rt::vec::Vec;

wit_bindgen::generate!({
    path: "./wit",
    world: "duckdb:extension/duckdb-extension-storage-write",
    // Bug 4b: also emit bindings for the sqlite:extension/{spi,types} import
    // wired into the storage-write world so we can call sqlite-lib's SPI.
    generate_all,
});

use duckdb::extension::{runtime, storage, types};
use exports::duckdb::extension::{callback_dispatch, guest, storage_dispatch, storage_write_dispatch};

use sqlite::extension::{spi as sqlite_spi, types as sqlite_types};

/// Opaque callback handle the host passes back to every storage-dispatch call.
const STORAGE_HANDLE: u32 = 1;
/// Opaque handle for the single registered `sqlite_blob_scan` table function.
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
// SPI adapter helpers. All SQL runs through sqlite:extension/spi; these turn
// sqlite errors into Duckerror, and duckvalues into sql-values (and back).
// The SPI holds ONE shared connection — `spi_owner_*` track which surface
// last opened it so we know when to reject a competing consumer.
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq)]
enum SpiOwner {
    Attach { catalog: u32, dsn: std::string::String },
    BlobScan,
}

thread_local! {
    static SPI_OWNER: RefCell<Option<SpiOwner>> = const { RefCell::new(None) };
}

fn spi_owner_get() -> Option<SpiOwner> {
    SPI_OWNER.with(|o| o.borrow().clone())
}

fn spi_owner_set(owner: Option<SpiOwner>) {
    SPI_OWNER.with(|o| *o.borrow_mut() = owner);
}

fn sqlite_err_to_duck(err: sqlite_types::SqliteError, context: &str) -> types::Duckerror {
    types::Duckerror::Io(format!(
        "{context}: sqlite [{}/{}] {}",
        err.code, err.extended_code, err.message
    ).into())
}

fn spi_execute(sql: &str) -> Result<sqlite_types::QueryResult, types::Duckerror> {
    sqlite_spi::execute(sql, &[]).map_err(|e| sqlite_err_to_duck(e, "spi.execute"))
}

fn spi_execute_params(
    sql: &str,
    params: &[sqlite_types::SqlValue],
) -> Result<sqlite_types::QueryResult, types::Duckerror> {
    sqlite_spi::execute(sql, params).map_err(|e| sqlite_err_to_duck(e, "spi.execute"))
}

fn spi_execute_batch(sql: &str) -> Result<(), types::Duckerror> {
    sqlite_spi::execute_batch(sql)
        .map(|_| ())
        .map_err(|e| sqlite_err_to_duck(e, "spi.execute_batch"))
}

// ---------------------------------------------------------------------------
// (a) sqlite_blob_scan table function  (the static-schema MELTED path)
// ---------------------------------------------------------------------------

impl callback_dispatch::Guest for Extension {
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
        let bytes: std::vec::Vec<u8> = match it.next() {
            Some(types::Duckvalue::Blob(b)) => b.into(),
            Some(types::Duckvalue::Text(s)) => hex_decode(&s).ok_or_else(|| {
                types::Duckerror::Invalidargument(
                    "sqlite_blob_scan: db string must be hex-encoded SQLite bytes".into(),
                )
            })?,
            Some(types::Duckvalue::Null) | None => {
                return Err(types::Duckerror::Invalidargument(
                    "sqlite_blob_scan: db argument is required".into(),
                ))
            }
            _ => {
                return Err(types::Duckerror::Invalidargument(
                    "sqlite_blob_scan: first argument must be a BLOB or hex string".into(),
                ))
            }
        };
        let table = match it.next() {
            Some(types::Duckvalue::Text(s)) => s.to_string(),
            _ => {
                return Err(types::Duckerror::Invalidargument(
                    "sqlite_blob_scan: second argument must be a table name (TEXT)".into(),
                ))
            }
        };

        // Blob-scan needs the SPI's shared connection as a fresh in-memory
        // deserialize target. Reject if an ATTACH already owns it — the two
        // surfaces can't share one connection.
        if let Some(SpiOwner::Attach { catalog, dsn }) = spi_owner_get() {
            return Err(types::Duckerror::Unsupported(format!(
                "sqlite_blob_scan cannot run while catalog {catalog} \
                 (attached from '{dsn}') is open — sqlite-lib's SPI has \
                 only one shared connection. DETACH the alias first."
            ).into()));
        }
        sqlite_spi::open_db("")
            .map_err(|e| sqlite_err_to_duck(e, "sqlite_blob_scan: open in-memory"))?;
        sqlite_spi::deserialize_db("main", &bytes)
            .map_err(|e| sqlite_err_to_duck(e, "sqlite_blob_scan: deserialize"))?;
        spi_owner_set(Some(SpiOwner::BlobScan));

        let rows = scan_melted(&table)?;
        Ok(rows.into())
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
fn scan_melted(table: &str) -> Result<std::vec::Vec<std::vec::Vec<types::Duckvalue>>, types::Duckerror> {
    let sql = format!("SELECT * FROM {}", quote_ident(table));
    let qr = spi_execute(&sql)?;
    let mut out: std::vec::Vec<std::vec::Vec<types::Duckvalue>> = std::vec::Vec::new();
    for (row_no, row) in qr.rows.iter().enumerate() {
        for (c, cell) in row.iter().enumerate() {
            let col_name = qr.columns.get(c).map(|s| s.as_str()).unwrap_or("?");
            let val = match cell {
                sqlite_types::SqlValue::Null => types::Duckvalue::Null,
                other => sqlite_value_as_text(other),
            };
            out.push(vec![
                types::Duckvalue::Int64(row_no as i64),
                types::Duckvalue::Text(std::string::String::from(col_name).into()),
                val,
            ]);
        }
    }
    Ok(out)
}

fn sqlite_value_as_text(v: &sqlite_types::SqlValue) -> types::Duckvalue {
    match v {
        sqlite_types::SqlValue::Null => types::Duckvalue::Null,
        sqlite_types::SqlValue::Integer(i) => types::Duckvalue::Text(i.to_string().into()),
        sqlite_types::SqlValue::Real(f) => types::Duckvalue::Text(f.to_string().into()),
        sqlite_types::SqlValue::Text(t) => types::Duckvalue::Text(t.clone().into()),
        sqlite_types::SqlValue::Blob(b) => {
            let mut s = std::string::String::with_capacity(b.len() * 2);
            for byte in b {
                s.push_str(&format!("{byte:02x}"));
            }
            types::Duckvalue::Text(s.into())
        }
    }
}

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

// ---------------------------------------------------------------------------
// (b) storage-dispatch: columnar projection + filter + limit pushdown
// ---------------------------------------------------------------------------

struct Cursor {
    rows: std::vec::Vec<std::vec::Vec<types::Duckvalue>>,
    pos: usize,
}

struct CatalogState {
    #[allow(dead_code)]
    dsn: std::string::String,
}

thread_local! {
    static STAGED: RefCell<HashMap<std::string::String, std::vec::Vec<u8>>> =
        RefCell::new(HashMap::new());
    static CATALOGS: RefCell<HashMap<u32, CatalogState>> = RefCell::new(HashMap::new());
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
        // v1: kept for API parity. The SPI opens by path, so staged bytes
        // never leave the wasm heap once storage_attach fires.
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
        // v1: single-connection SPI ⇒ single-attach.
        if let Some(existing) = spi_owner_get() {
            match existing {
                SpiOwner::Attach { catalog, dsn: prev_dsn } => {
                    return Err(types::Duckerror::Unsupported(format!(
                        "sqlitewasm supports only ONE attached catalog per session \
                         (already attached: catalog {catalog} -> '{prev_dsn}'). \
                         Multi-attach is a v-next follow-up needing per-connection \
                         isolation from sqlite-lib. DETACH the current alias first."
                    ).into()));
                }
                SpiOwner::BlobScan => {
                    spi_owner_set(None);
                }
            }
        }
        let dsn_str = dsn.to_string();
        sqlite_spi::open_db(&dsn_str)
            .map_err(|e| sqlite_err_to_duck(e, &format!("sqlitewasm: opening '{dsn_str}'")))?;
        let id = NEXT_CATALOG.with(|n| {
            let mut n = n.borrow_mut();
            let id = *n;
            *n += 1;
            id
        });
        CATALOGS.with(|c| c.borrow_mut().insert(id, CatalogState { dsn: dsn_str.clone() }));
        spi_owner_set(Some(SpiOwner::Attach { catalog: id, dsn: dsn_str }));
        STAGED.with(|s| { s.borrow_mut().remove(&dsn.to_string()); });
        Ok(id)
    }

    fn storage_list_tables(
        handle: u32,
        catalog: u32,
    ) -> Result<Vec<String>, types::Duckerror> {
        check_handle(handle)?;
        ensure_catalog_current(catalog)?;
        let qr = spi_execute("SELECT name FROM sqlite_master WHERE type='table' ORDER BY name")?;
        let mut out: Vec<String> = Vec::new();
        for row in qr.rows.iter() {
            if let Some(sqlite_types::SqlValue::Text(name)) = row.first() {
                out.push(name.clone().into());
            }
        }
        Ok(out)
    }

    fn storage_table_columns(
        handle: u32,
        catalog: u32,
        table: String,
    ) -> Result<Vec<types::Columndef>, types::Duckerror> {
        check_handle(handle)?;
        ensure_catalog_current(catalog)?;
        let cols = table_columns(&table)?;
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
    }

    fn storage_scan_open(
        handle: u32,
        catalog: u32,
        request: storage::ScanRequest,
    ) -> Result<u32, types::Duckerror> {
        check_handle(handle)?;
        ensure_catalog_current(catalog)?;
        let rows = run_scan(&request)?;
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
        if let Some(SpiOwner::Attach { catalog: cur, .. }) = spi_owner_get() {
            if cur == catalog {
                // Swap to in-memory so wasivfs releases any file handle.
                let _ = sqlite_spi::open_db("");
                spi_owner_set(None);
            }
        }
        Ok(true)
    }

    /// v1: the extension self-persists via wasivfs. Serialize is Unsupported —
    /// there's no in-memory copy to serialize, and re-reading the file just
    /// to hand bytes back to the host would double-write for zero gain.
    /// Combined with `writes-persist-directly = true`, the host skips its own
    /// write-back entirely.
    fn serialize(_handle: u32, _catalog: u32) -> Result<Vec<u8>, types::Duckerror> {
        Err(types::Duckerror::Unsupported(
            "sqlitewasm: writes persist directly via wasivfs; \
             serialize() would double-write and is not implemented".into(),
        ))
    }
}

// ---------------------------------------------------------------------------
// (c) storage-write-dispatch: transactions + DDL + DML (ADR Amendment A5)
// ---------------------------------------------------------------------------

thread_local! {
    static TXNS: RefCell<HashMap<u32, u32>> = RefCell::new(HashMap::new());
    static NEXT_TXN: RefCell<u32> = const { RefCell::new(1) };
}

impl storage_write_dispatch::Guest for Extension {
    fn begin_transaction(handle: u32, catalog: u32) -> Result<u32, types::Duckerror> {
        check_handle(handle)?;
        ensure_catalog_current(catalog)?;
        spi_execute_batch("BEGIN")?;
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
        ensure_catalog_current(catalog)?;
        spi_execute_batch("COMMIT")
    }

    fn rollback_transaction(handle: u32, txn: u32) -> Result<(), types::Duckerror> {
        check_handle(handle)?;
        let catalog = take_txn(txn)?;
        ensure_catalog_current(catalog)?;
        spi_execute_batch("ROLLBACK")
    }

    fn create_table(
        handle: u32,
        txn: u32,
        table: String,
        columns: Vec<types::Columndef>,
    ) -> Result<(), types::Duckerror> {
        check_handle(handle)?;
        let catalog = txn_catalog(txn)?;
        ensure_catalog_current(catalog)?;
        let col_defs: std::vec::Vec<std::string::String> = columns
            .iter()
            .map(|c| format!("{} {}", quote_ident(&c.name), logical_to_sqlite(&c.logical)))
            .collect();
        let sql = format!(
            "CREATE TABLE {} ({})",
            quote_ident(&table),
            col_defs.join(", ")
        );
        spi_execute_batch(&sql)
    }

    fn insert_rows(
        handle: u32,
        txn: u32,
        table: String,
        rows: Vec<Vec<types::Duckvalue>>,
    ) -> Result<u64, types::Duckerror> {
        check_handle(handle)?;
        let catalog = txn_catalog(txn)?;
        ensure_catalog_current(catalog)?;
        if rows.is_empty() {
            return Ok(0);
        }
        let cols = table_columns(&table)?;
        let n_underlying = cols.len();
        let width = rows[0].len();
        let strip_rowid = width == n_underlying + 1;
        if !strip_rowid && width != n_underlying {
            return Err(types::Duckerror::Invalidargument(format!(
                "insert-rows into '{table}': expected {n_underlying} or \
                 {} values per row, got {width}",
                n_underlying + 1
            )));
        }
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
            let payload: &[types::Duckvalue] = if strip_rowid { &row[1..] } else { &row[..] };
            let params: std::vec::Vec<sqlite_types::SqlValue> =
                payload.iter().map(duck_to_sqlite_value).collect();
            spi_execute_params(&sql, &params)?;
            inserted += 1;
        }
        Ok(inserted)
    }

    fn delete_rows(
        handle: u32,
        txn: u32,
        table: String,
        rowids: Vec<i64>,
    ) -> Result<u64, types::Duckerror> {
        check_handle(handle)?;
        let catalog = txn_catalog(txn)?;
        ensure_catalog_current(catalog)?;
        if rowids.is_empty() {
            return Ok(0);
        }
        let sql = format!("DELETE FROM {} WHERE rowid = ?", quote_ident(&table));
        let mut deleted: u64 = 0;
        for rid in &rowids {
            let params = vec![sqlite_types::SqlValue::Integer(*rid)];
            spi_execute_params(&sql, &params)?;
            deleted += 1;
        }
        Ok(deleted)
    }

    /// Bug 4b: writes persist DIRECTLY through wasivfs -> wasi:filesystem ->
    /// the ducklink host's preopens. No host-side write-back, no host-side
    /// rusqlite. The host sees `true` here and skips `at5_write_back`.
    fn writes_persist_directly(handle: u32) -> Result<bool, types::Duckerror> {
        check_handle(handle)?;
        Ok(true)
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
        ensure_catalog_current(catalog)?;
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
        let cols = table_columns(&table)?;
        let n_underlying = cols.len();
        let width = rows[0].len();
        let strip_rowid = width == n_underlying + 1;
        if !strip_rowid && width != n_underlying {
            return Err(types::Duckerror::Invalidargument(format!(
                "update-rows for '{table}': expected {n_underlying} or \
                 {} values per row, got {width}",
                n_underlying + 1
            )));
        }
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
            let payload: &[types::Duckvalue] = if strip_rowid { &row[1..] } else { &row[..] };
            let mut params: std::vec::Vec<sqlite_types::SqlValue> =
                payload.iter().map(duck_to_sqlite_value).collect();
            params.push(sqlite_types::SqlValue::Integer(*rid));
            spi_execute_params(&sql, &params)?;
            updated += 1;
        }
        Ok(updated)
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

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

fn ensure_catalog_current(catalog: u32) -> Result<(), types::Duckerror> {
    let owner = spi_owner_get();
    match owner {
        Some(SpiOwner::Attach { catalog: cur, .. }) if cur == catalog => {
            CATALOGS.with(|c| {
                if c.borrow().contains_key(&catalog) {
                    Ok(())
                } else {
                    Err(types::Duckerror::Invalidstate(
                        format!("unknown catalog {catalog}"),
                    ))
                }
            })
        }
        Some(SpiOwner::Attach { catalog: cur, .. }) => Err(types::Duckerror::Invalidstate(format!(
            "catalog {catalog} is not the currently-attached one ({cur})"
        ))),
        _ => Err(types::Duckerror::Invalidstate(
            format!("catalog {catalog} is no longer attached"),
        )),
    }
}

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

fn table_columns(
    table: &str,
) -> Result<std::vec::Vec<(std::string::String, types::Logicaltype)>, types::Duckerror> {
    let sql = format!("PRAGMA table_info({})", quote_ident(table));
    let qr = spi_execute(&sql)?;
    let mut out: std::vec::Vec<(std::string::String, types::Logicaltype)> = std::vec::Vec::new();
    for row in qr.rows.iter() {
        let name = match row.get(1) {
            Some(sqlite_types::SqlValue::Text(s)) => s.clone(),
            _ => continue,
        };
        let decl = match row.get(2) {
            Some(sqlite_types::SqlValue::Text(s)) => s.clone(),
            _ => std::string::String::new(),
        };
        out.push((name, map_decl_type(&decl)));
    }
    if out.is_empty() {
        return Err(types::Duckerror::Invalidargument(format!(
            "table '{table}' not found or has no columns"
        )));
    }
    Ok(out)
}

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

fn run_scan(
    request: &storage::ScanRequest,
) -> Result<std::vec::Vec<std::vec::Vec<types::Duckvalue>>, types::Duckerror> {
    let cols = table_columns(&request.table)?;
    let n_host_cols = cols.len() + 1;

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

    let mut binds: std::vec::Vec<sqlite_types::SqlValue> = std::vec::Vec::new();
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
                binds.push(duck_to_sqlite_value(&f.value));
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

    let qr = spi_execute_params(&sql, &binds)?;
    let mut out: std::vec::Vec<std::vec::Vec<types::Duckvalue>> =
        std::vec::Vec::with_capacity(qr.rows.len());
    for row in qr.rows.iter() {
        let mut emit: std::vec::Vec<types::Duckvalue> = std::vec::Vec::with_capacity(proj.len());
        for (slot, &host_idx) in proj.iter().enumerate() {
            let v = row.get(slot).cloned().unwrap_or(sqlite_types::SqlValue::Null);
            let rowid_logical = types::Logicaltype::Int64;
            let logical = if host_idx == 0 {
                &rowid_logical
            } else {
                &cols[host_idx - 1].1
            };
            emit.push(sqlite_value_to_duck(&v, logical));
        }
        out.push(emit);
    }
    Ok(out)
}

fn duck_to_sqlite_value(v: &types::Duckvalue) -> sqlite_types::SqlValue {
    match v {
        types::Duckvalue::Null => sqlite_types::SqlValue::Null,
        types::Duckvalue::Boolean(b) => sqlite_types::SqlValue::Integer(if *b { 1 } else { 0 }),
        types::Duckvalue::Int64(i) => sqlite_types::SqlValue::Integer(*i),
        types::Duckvalue::Uint64(u) => sqlite_types::SqlValue::Integer(*u as i64),
        types::Duckvalue::Float64(f) => sqlite_types::SqlValue::Real(*f),
        types::Duckvalue::Text(s) => sqlite_types::SqlValue::Text(s.to_string()),
        types::Duckvalue::Blob(b) => sqlite_types::SqlValue::Blob(b.to_vec()),
        types::Duckvalue::Int8(i) => sqlite_types::SqlValue::Integer(*i as i64),
        types::Duckvalue::Int16(i) => sqlite_types::SqlValue::Integer(*i as i64),
        types::Duckvalue::Int32(i) => sqlite_types::SqlValue::Integer(*i as i64),
        types::Duckvalue::Uint8(u) => sqlite_types::SqlValue::Integer(*u as i64),
        types::Duckvalue::Uint16(u) => sqlite_types::SqlValue::Integer(*u as i64),
        types::Duckvalue::Uint32(u) => sqlite_types::SqlValue::Integer(*u as i64),
        types::Duckvalue::Float32(f) => sqlite_types::SqlValue::Real(*f as f64),
        types::Duckvalue::Date(d) => sqlite_types::SqlValue::Integer(*d as i64),
        types::Duckvalue::Time(t) => sqlite_types::SqlValue::Integer(*t),
        types::Duckvalue::Timestamp(t) => sqlite_types::SqlValue::Integer(*t),
        types::Duckvalue::Timestamptz(t) => sqlite_types::SqlValue::Integer(*t),
        types::Duckvalue::Decimal(d) => sqlite_types::SqlValue::Integer(
            (((d.upper as u128) << 64) | d.lower as u128) as i64,
        ),
        types::Duckvalue::Interval(iv) => sqlite_types::SqlValue::Integer(iv.micros),
        types::Duckvalue::Uuid(u) => sqlite_types::SqlValue::Text(format!("{:016x}{:016x}", u.hi, u.lo)),
        types::Duckvalue::Hugeint(h) => {
            let v = ((h.upper as i128) << 64) | (h.lower as i128);
            sqlite_types::SqlValue::Text(v.to_string())
        }
        types::Duckvalue::Uhugeint(u) => {
            let v = ((u.upper as u128) << 64) | (u.lower as u128);
            sqlite_types::SqlValue::Text(v.to_string())
        }
        types::Duckvalue::Complex(c) => sqlite_types::SqlValue::Text(c.json.to_string()),
    }
}

fn sqlite_value_to_duck(v: &sqlite_types::SqlValue, ty: &types::Logicaltype) -> types::Duckvalue {
    match v {
        sqlite_types::SqlValue::Null => types::Duckvalue::Null,
        sqlite_types::SqlValue::Integer(i) => match ty {
            types::Logicaltype::Float64 => types::Duckvalue::Float64(*i as f64),
            types::Logicaltype::Text => types::Duckvalue::Text(i.to_string().into()),
            types::Logicaltype::Boolean => types::Duckvalue::Boolean(*i != 0),
            _ => types::Duckvalue::Int64(*i),
        },
        sqlite_types::SqlValue::Real(f) => match ty {
            types::Logicaltype::Int64 => types::Duckvalue::Int64(*f as i64),
            types::Logicaltype::Text => types::Duckvalue::Text(f.to_string().into()),
            _ => types::Duckvalue::Float64(*f),
        },
        sqlite_types::SqlValue::Text(t) => types::Duckvalue::Text(t.clone().into()),
        sqlite_types::SqlValue::Blob(b) => match ty {
            types::Logicaltype::Text => types::Duckvalue::Text(
                std::string::String::from_utf8_lossy(b).into_owned().into(),
            ),
            _ => types::Duckvalue::Blob(b.to_vec().into()),
        },
    }
}

fn quote_ident(name: &str) -> std::string::String {
    format!("\"{}\"", name.replace('"', "\"\""))
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
    storage::register_storage("sqlitewasm", STORAGE_HANDLE, None)?;
    Ok(())
}

export!(Extension);
