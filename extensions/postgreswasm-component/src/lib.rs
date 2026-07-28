//! PostgreSQL storage backend for DuckDB over wasi:sockets.
//!
//! A minimal hand-rolled PostgreSQL v3 wire-protocol client (see `postgres.rs`)
//! connects to a real server (plaintext; trust/cleartext/md5 auth) and serves
//! its tables through the storage / pushdown-scan WIT interface. This backs:
//!
//!     ATTACH '<dsn>' (TYPE postgreswasm);
//!
//! where `<dsn>` is either a URL `postgres://user:pw@host:port/db` or a
//! space/`;`-separated `host=.. port=.. user=.. password=.. database=..` string.
//!
//! Network access requires the host's network grant (DUCKLINK_NETWORK_GRANT).
//! Nothing panics across the FFI boundary -- every failure maps to a duckerror.
//!
//! ## rowid semantics: Postgres native `ctid`
//!
//! Amendment A5 requires every storage-dispatch extension that wants
//! UPDATE/DELETE support to advertise an integer `rowid` column that the
//! host's write pre-scan can extract and later hand back to
//! `storage-write-dispatch.update-rows` / `.delete-rows`. Postgres does not
//! have an integer rowid pseudo-column, but every heap-tuple has a **`ctid`**
//! (a `(block, offset)` tuple identifier) that IS unique within the table.
//! We pack it into an i64 as `((block as u64) << 16) | (offset as u64)`
//! (block fits in u32; offset fits in u16 — 48 bits total, comfortably in
//! positive i64 range).
//!
//! ### Caveats
//!
//! - `ctid` is a **physical** row identifier. VACUUM FULL, CLUSTER, and any
//!   `UPDATE` that rewrites the row invalidate old ctids. Within a single
//!   SQL statement (host does prescan -> immediate dispatch -> commit) this
//!   is fine; between statements a concurrent VACUUM FULL / CLUSTER can
//!   invalidate the packed rowid.
//! - The packing assumes block < 2^32 and offset < 2^16, which matches
//!   Postgres's on-disk representation (BlockNumber = uint32,
//!   OffsetNumber = uint16). Non-heap access methods that mint synthetic
//!   ctids outside those ranges would need a different mechanism.
//! - Custom access methods (e.g. columnar / compressed) that do not expose
//!   ctid at all will error out on `SELECT ctid`. This backend is designed
//!   for the default heap AM.
use std::cell::RefCell;
use std::collections::HashMap;

use wit_bindgen::rt::string::String;
use wit_bindgen::rt::vec::Vec;

mod postgres;
use postgres::{ColType, Column, PgConn};

wit_bindgen::generate!({ path: "./wit", world: "duckdb:extension/duckdb-extension-storage-write" });

use duckdb::extension::{storage, types};
use exports::duckdb::extension::{
    callback_dispatch, guest, storage_dispatch, storage_write_dispatch,
};

/// Opaque callback handle the host passes back to every storage-dispatch call.
const STORAGE_HANDLE: u32 = 1;

struct Extension;

impl guest::Guest for Extension {
    fn load() -> Result<types::Loadresult, types::Duckerror> {
        // Register the backend keyed by the ATTACH TYPE name "postgreswasm".
        // The alias "postgres" is registered too, but cannot collide with the
        // core's native postgres_scanner StorageExtension (which owns the
        // "postgres" type when embedded) -- the lean core de-embeds it, so the
        // "postgreswasm" name is the one the smoke test uses.
        storage::register_storage("postgreswasm", STORAGE_HANDLE, None)?;
        storage::register_storage("postgres", STORAGE_HANDLE, None)?;
        Ok(types::Loadresult {
            name: "postgreswasm".into(),
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
    // major-4 columnar dispatch: postgreswasm is a storage-only backend, so the
    // three columnar hot methods are Unsupported stubs.
    datalink_extcore::columnar_stub!();
    fn call_scalar(
        _h: u32,
        _a: Vec<types::Duckvalue>,
        _c: types::Invokeinfo,
    ) -> Result<types::Duckvalue, types::Duckerror> {
        Err(types::Duckerror::Unsupported("postgres: no scalar fns".into()))
    }
    fn call_table(
        _h: u32,
        _args: Vec<types::Duckvalue>,
    ) -> Result<types::Resultset, types::Duckerror> {
        Err(types::Duckerror::Unsupported("postgres: no table fns".into()))
    }
    fn call_pragma(
        _h: u32,
        _a: Vec<types::Duckvalue>,
    ) -> Result<Option<types::Duckvalue>, types::Duckerror> {
        Err(types::Duckerror::Unsupported("postgres: no pragmas".into()))
    }
    fn call_cast(
        _h: u32,
        _v: types::Duckvalue,
    ) -> Result<types::Duckvalue, types::Duckerror> {
        Err(types::Duckerror::Unsupported("postgres: no casts".into()))
    }
}

// ---------------------------------------------------------------------------
// storage-dispatch
// ---------------------------------------------------------------------------

/// Per-catalog state: a live connection plus a cache of table -> column list
/// (so a scan can resolve projection/filter indices to column names without a
/// round trip).
struct Catalog {
    conn: PgConn,
    columns: HashMap<std::string::String, std::vec::Vec<Column>>,
}

struct Cursor {
    rows: std::vec::Vec<std::vec::Vec<types::Duckvalue>>,
    pos: usize,
}

thread_local! {
    static CATALOGS: RefCell<HashMap<u32, Catalog>> = RefCell::new(HashMap::new());
    static SCANS: RefCell<HashMap<u32, Cursor>> = RefCell::new(HashMap::new());
    static NEXT_CATALOG: RefCell<u32> = const { RefCell::new(1) };
    static NEXT_SCAN: RefCell<u32> = const { RefCell::new(1) };
    /// Open write-transactions keyed by txn-id. Each entry stores the
    /// catalog-id the transaction was begun on, so the write-side calls
    /// (insert/update/delete/create-table) can find the right PgConn
    /// under the CATALOGS map without the host having to hand
    /// the catalog back on every call.
    static TXNS: RefCell<HashMap<u32, u32>> = RefCell::new(HashMap::new());
    static NEXT_TXN: RefCell<u32> = const { RefCell::new(1) };
}

impl storage_dispatch::Guest for Extension {
    /// Not used for postgres (the dsn is a connection string, not a file blob).
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
        let conn = PgConn::connect(
            &cfg.host,
            cfg.port,
            &cfg.user,
            &cfg.password,
            &cfg.database,
        )
        .map_err(|e| types::Duckerror::Io(format!("postgres attach: {}", e.0)))?;
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
            let sql = "SELECT tablename FROM pg_catalog.pg_tables \
                       WHERE schemaname NOT IN ('pg_catalog','information_schema') \
                       ORDER BY tablename";
            let rs = cat
                .conn
                .query(sql)
                .map_err(|e| types::Duckerror::Io(format!("list tables: {}", e.0)))?;
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
            // index 0 so the host's `at5_locate_rowid_column` finds it and
            // the write-path pre-scan can extract the packed ctid value from
            // the emitted rows (see docs/at5-rowid-mechanism.md). The value
            // in that slot is `((ctid.block as u64) << 16) | ctid.offset`.
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
            // wasm32: `usize` is 32-bit, so `cur.pos + max_rows` (with e.g.
            // `max_rows = u32::MAX`, which the host uses to mean "drain
            // everything") wraps. Mirror sqlitewasm's saturating_add + explicit
            // EOF short-circuit so the drain loop terminates cleanly.
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
        // Dropping the Catalog drops the TcpStream, closing the connection.
        CATALOGS.with(|c| c.borrow_mut().remove(&catalog));
        Ok(true)
    }

    /// AT5 write-back stub: Postgres is a live network connection, not a
    /// serializable blob. The host's `at5_write_back` treats
    /// `Duckerror::Unsupported` as "backend already round-tripped the
    /// mutation over the wire; skip the fs write" -- exactly the semantics
    /// we want here.
    fn serialize(handle: u32, _catalog: u32) -> Result<Vec<u8>, types::Duckerror> {
        check_handle(handle)?;
        Err(types::Duckerror::Unsupported(
            "postgreswasm: mutations persist over the wire; no blob to serialize"
                .into(),
        ))
    }
}

// ---------------------------------------------------------------------------
// storage-write-dispatch: transactions + DDL + DML (ADR Amendment A5)
// ---------------------------------------------------------------------------

impl storage_write_dispatch::Guest for Extension {
    fn begin_transaction(handle: u32, catalog: u32) -> Result<u32, types::Duckerror> {
        check_handle(handle)?;
        with_catalog(catalog, |cat| {
            cat.conn
                .query("BEGIN")
                .map(|_| ())
                .map_err(|e| types::Duckerror::Io(format!("BEGIN: {}", e.0)))
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
                .map(|_| ())
                .map_err(|e| types::Duckerror::Io(format!("COMMIT: {}", e.0)))
        })
    }

    fn rollback_transaction(handle: u32, txn: u32) -> Result<(), types::Duckerror> {
        check_handle(handle)?;
        let catalog = take_txn(txn)?;
        with_catalog(catalog, |cat| {
            cat.conn
                .query("ROLLBACK")
                .map(|_| ())
                .map_err(|e| types::Duckerror::Io(format!("ROLLBACK: {}", e.0)))
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
        // Skip the leading synthetic `rowid` slot if it was echoed back:
        // Postgres mints its own tuple ctids; we do not persist a duck rowid.
        let mut col_slice: &[types::Columndef] = &columns;
        if let Some(first) = columns.first() {
            if first.name.eq_ignore_ascii_case("rowid") {
                col_slice = &columns[1..];
            }
        }
        let col_defs: std::vec::Vec<std::string::String> = col_slice
            .iter()
            .map(|c| format!("{} {}", quote_ident(&c.name), logical_to_postgres(&c.logical)))
            .collect();
        let sql = format!(
            "CREATE TABLE {} ({})",
            quote_ident(&table),
            col_defs.join(", ")
        );
        with_catalog(catalog, |cat| {
            cat.conn
                .query(&sql)
                .map(|_| ())
                .map_err(|e| types::Duckerror::Io(format!("CREATE TABLE: {}", e.0)))
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
        // Rows arrive at the extension's advertised width (rowid + underlying
        // cols) per ADR Amendment A5. Strip the leading rowid slot: an
        // INSERT lets Postgres mint the ctid itself.
        let underlying_cols = with_catalog(catalog, |cat| Ok(fetch_columns(cat, &table)?.clone()))?;
        let n_underlying = underlying_cols.len();
        let width = rows[0].len();
        let strip_rowid = width == n_underlying + 1;
        if !strip_rowid && width != n_underlying {
            return Err(types::Duckerror::Invalidargument(format!(
                "insert-rows into '{table}': expected {n_underlying} or \
                 {} values per row, got {width}",
                n_underlying + 1
            )));
        }
        let col_list: std::vec::Vec<std::string::String> = underlying_cols
            .iter()
            .map(|c| quote_ident(&c.name))
            .collect();
        with_catalog(catalog, |cat| {
            let mut inserted: u64 = 0;
            for row in &rows {
                let payload: &[types::Duckvalue] = if strip_rowid {
                    &row[1..]
                } else {
                    &row[..]
                };
                let vals: std::vec::Vec<std::string::String> =
                    payload.iter().map(literal).collect();
                let sql = format!(
                    "INSERT INTO {} ({}) VALUES ({})",
                    quote_ident(&table),
                    col_list.join(", "),
                    vals.join(", "),
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
            for rid in &rowids {
                let (block, off) = unpack_ctid(*rid);
                let sql = format!(
                    "DELETE FROM {} WHERE ctid = '({block},{off})'::tid",
                    quote_ident(&table),
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
        let underlying_cols = with_catalog(catalog, |cat| Ok(fetch_columns(cat, &table)?.clone()))?;
        let n_underlying = underlying_cols.len();
        let width = rows[0].len();
        // The intercept_write path passes ROWS at the extension's advertised
        // width (rowid + underlying cols). The rowid slot at index 0 is
        // ignored -- WHERE ctid = ...::tid routes the write, not the payload.
        let strip_rowid = width == n_underlying + 1;
        if !strip_rowid && width != n_underlying {
            return Err(types::Duckerror::Invalidargument(format!(
                "update-rows for '{table}': expected {n_underlying} or \
                 {} values per row, got {width}",
                n_underlying + 1
            )));
        }
        with_catalog(catalog, |cat| {
            let mut updated: u64 = 0;
            for (rid, row) in rowids.iter().zip(rows.iter()) {
                let payload: &[types::Duckvalue] = if strip_rowid {
                    &row[1..]
                } else {
                    &row[..]
                };
                let sets: std::vec::Vec<std::string::String> = underlying_cols
                    .iter()
                    .zip(payload.iter())
                    .map(|(c, v)| format!("{} = {}", quote_ident(&c.name), literal(v)))
                    .collect();
                let (block, off) = unpack_ctid(*rid);
                let sql = format!(
                    "UPDATE {} SET {} WHERE ctid = '({block},{off})'::tid",
                    quote_ident(&table),
                    sets.join(", "),
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

/// Map an extension logicaltype to a Postgres column type for CREATE TABLE.
fn logical_to_postgres(ty: &types::Logicaltype) -> &'static str {
    match ty {
        types::Logicaltype::Boolean => "BOOLEAN",
        types::Logicaltype::Int8 | types::Logicaltype::Int16 => "SMALLINT",
        types::Logicaltype::Int32 => "INTEGER",
        types::Logicaltype::Int64 => "BIGINT",
        // Postgres has no unsigned types; widen one step (Uint32 fits BIGINT,
        // Uint64 doesn't -- the caller loses one bit of range on Uint64).
        types::Logicaltype::Uint8 | types::Logicaltype::Uint16 => "SMALLINT",
        types::Logicaltype::Uint32 | types::Logicaltype::Uint64 => "BIGINT",
        types::Logicaltype::Float32 => "REAL",
        types::Logicaltype::Float64 => "DOUBLE PRECISION",
        types::Logicaltype::Blob => "BYTEA",
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

/// Resolve a table's columns, caching the result on the catalog. Uses
/// information_schema.columns and maps data_type text -> logical type.
fn fetch_columns<'a>(
    cat: &'a mut Catalog,
    table: &str,
) -> Result<&'a std::vec::Vec<Column>, types::Duckerror> {
    if !cat.columns.contains_key(table) {
        let sql = format!(
            "SELECT column_name, data_type FROM information_schema.columns \
             WHERE table_name = {} ORDER BY ordinal_position",
            quote_str(table)
        );
        let rs = cat
            .conn
            .query(&sql)
            .map_err(|e| types::Duckerror::Io(format!("table columns: {}", e.0)))?;
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

/// Build + run the pushdown query, materializing rows mapped to logical types.
fn run_scan(
    cat: &mut Catalog,
    request: &storage::ScanRequest,
) -> Result<std::vec::Vec<std::vec::Vec<types::Duckvalue>>, types::Duckerror> {
    let cols: std::vec::Vec<Column> = fetch_columns(cat, &request.table)?.clone();
    // Host-side indices are 1-based into Postgres's columns: index 0 is the
    // synthetic `rowid` slot (ctid packed as s64) advertised by
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

    // Postgres has no int8 rowid pseudo-column, but every heap row has a
    // `ctid` (a `(block,offset)` tuple identifier). We SELECT ctid alongside
    // the projected columns and pack it into an i64 when materializing the
    // batch below. The projection may reference the rowid slot at index 0
    // multiple times -- de-duplicate the SQL SELECT and remember the mapping
    // so the emit loop can copy the ctid into every rowid slot.
    let mut sql_cols: std::vec::Vec<std::string::String> = std::vec::Vec::new();
    let mut projection_slot_to_sql_slot: std::vec::Vec<usize> =
        std::vec::Vec::with_capacity(proj.len());
    let mut ctid_sql_slot: Option<usize> = None;
    for &host_idx in &proj {
        if host_idx == 0 {
            if let Some(s) = ctid_sql_slot {
                projection_slot_to_sql_slot.push(s);
            } else {
                let s = sql_cols.len();
                sql_cols.push("ctid".to_string());
                ctid_sql_slot = Some(s);
                projection_slot_to_sql_slot.push(s);
            }
        } else {
            let s = sql_cols.len();
            sql_cols.push(quote_ident(&cols[host_idx - 1].name));
            projection_slot_to_sql_slot.push(s);
        }
    }
    let mut sql = format!(
        "SELECT {} FROM {}",
        sql_cols.join(", "),
        quote_ident(&request.table)
    );

    // WHERE: AND-join the filters, binding values inline (numbers raw, strings
    // single-quoted with '' escaping). Filters on the synthetic rowid column
    // (index 0) turn into `ctid = '(block,off)'::tid` predicates; other ops
    // on the rowid slot are declined (the host re-applies filters anyway).
    let mut conds: std::vec::Vec<std::string::String> = std::vec::Vec::new();
    for fltr in &request.filters {
        let idx = fltr.column as usize;
        if idx >= n_host_cols {
            return Err(types::Duckerror::Invalidargument(
                "filter column index out of range".into(),
            ));
        }
        if idx == 0 {
            // A rowid EQ filter can be lowered to a ctid predicate; skip
            // anything else and let the host re-apply.
            if let (storage::CompareOp::Eq, types::Duckvalue::Int64(v)) =
                (fltr.op, &fltr.value)
            {
                let (block, off) = unpack_ctid(*v);
                conds.push(format!("ctid = '({block},{off})'::tid"));
            }
            continue;
        }
        let col = quote_ident(&cols[idx - 1].name);
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
    for row in &rs.rows {
        let mut emit: std::vec::Vec<types::Duckvalue> = std::vec::Vec::with_capacity(proj.len());
        for (slot, &host_idx) in proj.iter().enumerate() {
            let sql_slot = projection_slot_to_sql_slot[slot];
            let cell = row.get(sql_slot).and_then(|c| c.as_ref());
            if host_idx == 0 {
                // Parse `(block,offset)` and pack to i64. A ctid we can't
                // parse -> Null (the host's `at5_duckvalue_to_i64` will
                // reject it, so this only affects `SELECT rowid` for rows
                // that came from a non-heap AM).
                emit.push(match cell.and_then(|s| parse_ctid(s)) {
                    Some((block, off)) => types::Duckvalue::Int64(pack_ctid(block, off)),
                    None => types::Duckvalue::Null,
                });
            } else {
                let ci = host_idx - 1;
                emit.push(text_to_duck(cell, cols[ci].ty));
            }
        }
        out.push(emit);
    }
    Ok(out)
}

// ---------------------------------------------------------------------------
// ctid <-> i64 packing
// ---------------------------------------------------------------------------

/// Pack a Postgres ctid `(block, offset)` into a positive i64. The layout is
/// `(block as u64) << 16 | offset` — block fits in 32 bits, offset in 16, so
/// the total is 48 bits and always fits in the positive i64 range.
pub(crate) fn pack_ctid(block: u32, offset: u16) -> i64 {
    (((block as u64) << 16) | (offset as u64)) as i64
}

/// Reverse of `pack_ctid`.
pub(crate) fn unpack_ctid(v: i64) -> (u32, u16) {
    let u = v as u64;
    ((u >> 16) as u32, (u & 0xFFFF) as u16)
}

/// Parse Postgres's text-formatted ctid `(block,offset)` into a `(u32, u16)`
/// pair. Whitespace and non-matching input -> None.
pub(crate) fn parse_ctid(s: &str) -> Option<(u32, u16)> {
    let inner = s.trim().strip_prefix('(')?.strip_suffix(')')?;
    let (b, o) = inner.split_once(',')?;
    let block: u32 = b.trim().parse().ok()?;
    let offset: u16 = o.trim().parse().ok()?;
    Some((block, offset))
}

// ---------------------------------------------------------------------------
// value + type mapping
// ---------------------------------------------------------------------------

fn coltype_to_logical(ty: ColType) -> types::Logicaltype {
    match ty {
        ColType::Int => types::Logicaltype::Int64,
        ColType::Float => types::Logicaltype::Float64,
        ColType::Bool => types::Logicaltype::Boolean,
        ColType::Text => types::Logicaltype::Text,
    }
}

/// Classify an information_schema.columns `data_type` text
/// (e.g. "integer", "bigint", "double precision", "numeric", "boolean", "text").
fn classify_decl(decl: &str) -> ColType {
    let d = decl.trim().to_ascii_lowercase();
    match d.as_str() {
        "integer" | "bigint" | "smallint" => ColType::Int,
        "double precision" | "real" | "numeric" | "decimal" => ColType::Float,
        "boolean" => ColType::Bool,
        _ => ColType::Text,
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
        ColType::Bool => match s.as_str() {
            // postgres TEXT format for bool is 't' / 'f'.
            "t" | "true" | "T" | "1" => types::Duckvalue::Boolean(true),
            "f" | "false" | "F" | "0" => types::Duckvalue::Boolean(false),
            _ => types::Duckvalue::Text(s.clone().into()),
        },
        ColType::Text => types::Duckvalue::Text(s.clone().into()),
    }
}

/// Render a duckvalue as an inline SQL literal (minimal escaping).
fn literal(v: &types::Duckvalue) -> std::string::String {
    match v {
        types::Duckvalue::Null => "NULL".to_string(),
        types::Duckvalue::Boolean(b) => if *b { "TRUE" } else { "FALSE" }.to_string(),
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

/// Single-quote a string literal, doubling embedded single quotes (PostgreSQL
/// standard-conforming-strings: a backslash is NOT an escape inside ''-quotes).
fn quote_str(s: &str) -> std::string::String {
    format!("'{}'", s.replace('\'', "''"))
}

/// Double-quote an identifier, doubling embedded double quotes.
fn quote_ident(name: &str) -> std::string::String {
    format!("\"{}\"", name.replace('"', "\"\""))
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
/// `postgres://user:pw@host:port/db` and a key=value form
/// `host=.. port=.. user=.. password=.. database=..` (whitespace or `;`
/// separated). Defaults: host 127.0.0.1, port 5432, empty password.
fn parse_dsn(dsn: &str) -> Result<DsnConfig, types::Duckerror> {
    let dsn = dsn.trim();
    let mut cfg = DsnConfig {
        host: "127.0.0.1".to_string(),
        port: 5432,
        user: std::string::String::new(),
        password: std::string::String::new(),
        database: std::string::String::new(),
    };

    if let Some(rest) = dsn
        .strip_prefix("postgresql://")
        .or_else(|| dsn.strip_prefix("postgres://"))
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
            "postgres dsn: missing user".into(),
        ));
    }
    Ok(cfg)
}

export!(Extension);

// ---------------------------------------------------------------------------
// Native unit tests (DSN parsing + type mapping + ctid packing; no network).
// ---------------------------------------------------------------------------
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dsn_keyvalue() {
        let c = parse_dsn("host=127.0.0.1 port=5433 user=postgres database=ducktest").unwrap();
        assert_eq!(c.host, "127.0.0.1");
        assert_eq!(c.port, 5433);
        assert_eq!(c.user, "postgres");
        assert_eq!(c.password, "");
        assert_eq!(c.database, "ducktest");
    }

    #[test]
    fn dsn_keyvalue_with_password() {
        let c = parse_dsn("host=db port=5432 user=u password=p dbname=d").unwrap();
        assert_eq!(c.host, "db");
        assert_eq!(c.user, "u");
        assert_eq!(c.password, "p");
        assert_eq!(c.database, "d");
    }

    #[test]
    fn dsn_url() {
        let c = parse_dsn("postgres://duck:duckpw@127.0.0.1:5433/ducktest").unwrap();
        assert_eq!(c.host, "127.0.0.1");
        assert_eq!(c.port, 5433);
        assert_eq!(c.user, "duck");
        assert_eq!(c.password, "duckpw");
        assert_eq!(c.database, "ducktest");
    }

    #[test]
    fn dsn_url_defaults() {
        let c = parse_dsn("postgresql://duck@host/db").unwrap();
        assert_eq!(c.host, "host");
        assert_eq!(c.port, 5432); // default postgres port
        assert_eq!(c.user, "duck");
        assert_eq!(c.password, "");
        assert_eq!(c.database, "db");
    }

    #[test]
    fn dsn_missing_user_errors() {
        assert!(parse_dsn("host=127.0.0.1 port=5433").is_err());
    }

    #[test]
    fn classify_types() {
        assert_eq!(classify_decl("integer"), ColType::Int);
        assert_eq!(classify_decl("bigint"), ColType::Int);
        assert_eq!(classify_decl("smallint"), ColType::Int);
        assert_eq!(classify_decl("double precision"), ColType::Float);
        assert_eq!(classify_decl("numeric"), ColType::Float);
        assert_eq!(classify_decl("real"), ColType::Float);
        assert_eq!(classify_decl("boolean"), ColType::Bool);
        assert_eq!(classify_decl("text"), ColType::Text);
        assert_eq!(classify_decl("character varying"), ColType::Text);
    }

    #[test]
    fn literal_escaping() {
        assert_eq!(literal(&types::Duckvalue::Int64(5)), "5");
        assert_eq!(literal(&types::Duckvalue::Text("a'b".into())), "'a''b'");
        assert_eq!(literal(&types::Duckvalue::Boolean(true)), "TRUE");
    }

    #[test]
    fn ident_quoting() {
        assert_eq!(quote_ident("t"), "\"t\"");
        assert_eq!(quote_ident("a\"b"), "\"a\"\"b\"");
    }

    // `Duckvalue` (from the WIT macro) doesn't derive PartialEq, so match the
    // produced variant explicitly rather than assert_eq.
    #[test]
    fn bool_text_parse() {
        assert!(matches!(
            text_to_duck(Some(&"t".to_string()), ColType::Bool),
            types::Duckvalue::Boolean(true)
        ));
        assert!(matches!(
            text_to_duck(Some(&"f".to_string()), ColType::Bool),
            types::Duckvalue::Boolean(false)
        ));
        assert!(matches!(
            text_to_duck(None, ColType::Int),
            types::Duckvalue::Null
        ));
        assert!(matches!(
            text_to_duck(Some(&"7".to_string()), ColType::Int),
            types::Duckvalue::Int64(7)
        ));
    }

    #[test]
    fn ctid_pack_roundtrip() {
        for (block, offset) in [(0u32, 1u16), (1, 2), (42, 7), (u32::MAX, u16::MAX)] {
            let v = pack_ctid(block, offset);
            assert!(v >= 0, "pack_ctid({block},{offset}) must be positive");
            assert_eq!(unpack_ctid(v), (block, offset));
        }
    }

    #[test]
    fn ctid_parse_matches_pg_text_format() {
        assert_eq!(parse_ctid("(0,1)"), Some((0u32, 1u16)));
        assert_eq!(parse_ctid("(42,7)"), Some((42, 7)));
        assert_eq!(parse_ctid("  (12,3)\n"), Some((12, 3)));
        assert_eq!(parse_ctid("(0, 1)"), Some((0, 1)));
        // Non-heap AMs might produce values we can't parse; return None,
        // never panic. The read path emits Null in the rowid slot then.
        assert_eq!(parse_ctid(""), None);
        assert_eq!(parse_ctid("bogus"), None);
        assert_eq!(parse_ctid("(1,"), None);
        assert_eq!(parse_ctid("(1)"), None);
    }

    #[test]
    fn logical_to_postgres_maps_expected() {
        assert_eq!(logical_to_postgres(&types::Logicaltype::Int64), "BIGINT");
        assert_eq!(logical_to_postgres(&types::Logicaltype::Int32), "INTEGER");
        assert_eq!(logical_to_postgres(&types::Logicaltype::Text), "TEXT");
        assert_eq!(logical_to_postgres(&types::Logicaltype::Boolean), "BOOLEAN");
        assert_eq!(
            logical_to_postgres(&types::Logicaltype::Float64),
            "DOUBLE PRECISION"
        );
        assert_eq!(logical_to_postgres(&types::Logicaltype::Blob), "BYTEA");
    }
}
