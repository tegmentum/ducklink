//! Unity Catalog storage backend for DuckDB over wasi:sockets.
//!
//! A REST client (UC `/api/2.1/unity-catalog/...`) enumerates a Unity Catalog's
//! schemas, tables and columns and serves them through the storage / pushdown
//! WIT interface. This backs:
//!
//!     ATTACH 'https://<host>;token=<pat>;catalog=<cat>' AS uc (TYPE unity);
//!
//! WHAT IS REAL (the deliverable): the catalog ENUMERATION over REST -
//! storage-list-tables walks /schemas + /tables, storage-table-columns reads a
//! table's columns[] -> DuckDB columndefs. The JSON parsing is exhaustively
//! unit-tested offline (see `uc.rs`).
//!
//! THE DATA SCAN (documented, out of scope here): a UC table points to a
//! `storage_location` (s3://, az://) holding delta/parquet files. A full scan
//! would hand that location to the s3fs/azfs file systems + the delta/parquet
//! readers. Without the data FS in this component, storage-scan returns ZERO
//! rows; the metadata path (names + columns) is the solid, testable surface.
//!
//! Network access requires the host's network grant (DUCKLINK_NETWORK_GRANT).
//! Nothing panics across the FFI boundary -- every failure maps to a duckerror.
use std::cell::RefCell;
use std::collections::HashMap;

use wit_bindgen::rt::string::String;
use wit_bindgen::rt::vec::Vec;

mod http;
mod uc;
use uc::{UcColumn, UcConfig, UcTable};

wit_bindgen::generate!({ path: "./wit", world: "duckdb:extension/duckdb-extension-storage" });

use duckdb::extension::{storage, types};
use exports::duckdb::extension::{callback_dispatch, guest, storage_dispatch};

/// Opaque callback handle the host passes back to every storage-dispatch call.
const STORAGE_HANDLE: u32 = 1;

struct Extension;

impl guest::Guest for Extension {
    fn load() -> Result<types::Loadresult, types::Duckerror> {
        // Register the backend keyed by the ATTACH TYPE name "unity". Also a
        // "unityscan" alias that cannot collide with the core's native
        // unity_catalog StorageExtension if the lean core ever ships it.
        storage::register_storage("unity", STORAGE_HANDLE, None)?;
        storage::register_storage("unityscan", STORAGE_HANDLE, None)?;
        Ok(types::Loadresult {
            name: "unityscan".into(),
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

// No functions; the callback-dispatch export is required by the world but every
// entry is unsupported.
impl callback_dispatch::Guest for Extension {
    // major-4 columnar dispatch: unityscan is a storage backend, so the three
    // columnar hot methods are Unsupported stubs.
    datalink_extcore::columnar_stub!();

    fn call_scalar(
        _h: u32,
        _a: Vec<types::Duckvalue>,
        _c: types::Invokeinfo,
    ) -> Result<types::Duckvalue, types::Duckerror> {
        Err(types::Duckerror::Unsupported("unity: no scalar fns".into()))
    }
    fn call_table(
        _h: u32,
        _args: Vec<types::Duckvalue>,
    ) -> Result<types::Resultset, types::Duckerror> {
        Err(types::Duckerror::Unsupported("unity: no table fns".into()))
    }
    fn call_pragma(
        _h: u32,
        _a: Vec<types::Duckvalue>,
    ) -> Result<Option<types::Duckvalue>, types::Duckerror> {
        Err(types::Duckerror::Unsupported("unity: no pragmas".into()))
    }
    fn call_cast(
        _h: u32,
        _v: types::Duckvalue,
    ) -> Result<types::Duckvalue, types::Duckerror> {
        Err(types::Duckerror::Unsupported("unity: no casts".into()))
    }
}

// ---------------------------------------------------------------------------
// storage-dispatch
// ---------------------------------------------------------------------------

/// Per-catalog state: the UC endpoint config plus a cache of fetched tables
/// keyed by their full "schema.table" name (so columns/scan resolve without an
/// extra round trip after list-tables).
struct Catalog {
    cfg: UcConfig,
    tables: HashMap<std::string::String, UcTable>,
}

thread_local! {
    static CATALOGS: RefCell<HashMap<u32, Catalog>> = RefCell::new(HashMap::new());
    static NEXT_CATALOG: RefCell<u32> = const { RefCell::new(1) };
    static NEXT_SCAN: RefCell<u32> = const { RefCell::new(1) };
}

impl storage_dispatch::Guest for Extension {
    /// Not used for unity (the dsn is a REST endpoint, not a file blob).
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
        options: Vec<(String, String)>,
    ) -> Result<u32, types::Duckerror> {
        check_handle(handle)?;
        let mut cfg = uc::parse_dsn(&dsn).map_err(types::Duckerror::Invalidargument)?;
        // ATTACH options override DSN keys (e.g. (TOKEN '...', CATALOG '...')).
        for (k, v) in &options {
            match k.to_ascii_lowercase().as_str() {
                "token" | "bearer" | "pat" => cfg.token = v.to_string(),
                "catalog" | "catalog_name" => cfg.catalog = v.to_string(),
                "endpoint" | "url" => cfg.endpoint = v.to_string(),
                _ => {}
            }
        }
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
                    cfg,
                    tables: HashMap::new(),
                },
            )
        });
        Ok(id)
    }

    /// Walk /schemas then /tables for each schema, returning fully-qualified
    /// "schema.table" names. The fetched UcTable (columns + storage_location) is
    /// cached so storage-table-columns needs no further round trip.
    fn storage_list_tables(
        handle: u32,
        catalog: u32,
    ) -> Result<Vec<String>, types::Duckerror> {
        check_handle(handle)?;
        CATALOGS.with(|c| {
            let mut c = c.borrow_mut();
            let cat = c
                .get_mut(&catalog)
                .ok_or_else(|| types::Duckerror::Invalidstate("unknown catalog".into()))?;

            let token = opt_token(&cat.cfg);
            let schemas_url = uc::schemas_url(&cat.cfg.endpoint, &cat.cfg.catalog);
            let body = http::get(&schemas_url, token)
                .map_err(|e| types::Duckerror::Io(format!("UC /schemas: {e}")))?;
            let schemas = uc::parse_schemas(&body)
                .map_err(|e| types::Duckerror::Io(format!("UC /schemas parse: {e}")))?;

            let mut out: Vec<String> = Vec::new();
            for sch in &schemas {
                let turl = uc::tables_url(&cat.cfg.endpoint, &cat.cfg.catalog, &sch.name);
                let tbody = http::get(&turl, token)
                    .map_err(|e| types::Duckerror::Io(format!("UC /tables: {e}")))?;
                let tables = uc::parse_tables(&tbody, &sch.name)
                    .map_err(|e| types::Duckerror::Io(format!("UC /tables parse: {e}")))?;
                for t in tables {
                    let full = format!("{}.{}", sch.name, t.name);
                    out.push(full.clone().into());
                    cat.tables.insert(full, t);
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
        CATALOGS.with(|c| {
            let mut c = c.borrow_mut();
            let cat = c
                .get_mut(&catalog)
                .ok_or_else(|| types::Duckerror::Invalidstate("unknown catalog".into()))?;
            let cols = resolve_columns(cat, &table)?;
            Ok(cols
                .iter()
                .map(|col| types::Columndef {
                    name: col.name.clone().into(),
                    logical: map_uc_type(&col.type_text),
                })
                .collect())
        })
    }

    /// Open a scan. The catalog ENUMERATION is the deliverable; a real data scan
    /// would fetch the table's `storage_location` (delta/parquet on s3/az) via
    /// s3fs/azfs + the delta/parquet readers, which are NOT part of this
    /// component. We validate the table resolves (columns reachable) and return
    /// an empty cursor -> zero rows.
    fn storage_scan_open(
        handle: u32,
        catalog: u32,
        request: storage::ScanRequest,
    ) -> Result<u32, types::Duckerror> {
        check_handle(handle)?;
        CATALOGS.with(|c| {
            let mut c = c.borrow_mut();
            let cat = c
                .get_mut(&catalog)
                .ok_or_else(|| types::Duckerror::Invalidstate("unknown catalog".into()))?;
            // Resolve to confirm the table exists (and warm the cache); ignore
            // the columns -- the scan itself yields no rows here.
            resolve_columns(cat, &request.table)?;
            Ok(())
        })?;
        let id = NEXT_SCAN.with(|n| {
            let mut n = n.borrow_mut();
            let id = *n;
            *n += 1;
            id
        });
        Ok(id)
    }

    /// Always EOF: the data files behind the table's storage_location are read
    /// by the composed s3fs/azfs + delta/parquet stack, not here.
    fn storage_scan_next(
        handle: u32,
        _scan: u32,
        _max_rows: u32,
    ) -> Result<types::Resultset, types::Duckerror> {
        check_handle(handle)?;
        let empty: std::vec::Vec<std::vec::Vec<types::Duckvalue>> = std::vec::Vec::new();
        Ok(empty.into())
    }

    fn storage_scan_close(handle: u32, _scan: u32) -> Result<bool, types::Duckerror> {
        check_handle(handle)?;
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

fn opt_token(cfg: &UcConfig) -> Option<&str> {
    if cfg.token.is_empty() {
        None
    } else {
        Some(cfg.token.as_str())
    }
}

/// Resolve a table's columns. Tries the list-tables cache first (keyed by the
/// "schema.table" full name); on a miss it does a direct GET /tables/{full_name}
/// (qualified with the catalog when the caller passed only "schema.table").
fn resolve_columns<'a>(
    cat: &'a mut Catalog,
    table: &str,
) -> Result<&'a std::vec::Vec<UcColumn>, types::Duckerror> {
    if !cat.tables.contains_key(table) {
        // Build the UC full name "catalog.schema.table". The host hands us a
        // "schema.table" (our list-tables key); prefix the catalog for the REST
        // path. If only a bare table name arrives, leave it and let UC resolve.
        let full_name = if table.matches('.').count() >= 2 {
            table.to_string()
        } else {
            format!("{}.{}", cat.cfg.catalog, table)
        };
        let url = uc::table_url(&cat.cfg.endpoint, &full_name);
        let body = http::get(&url, opt_token(&cat.cfg))
            .map_err(|e| types::Duckerror::Io(format!("UC /tables/{table}: {e}")))?;
        let t = uc::parse_table(&body)
            .map_err(|e| types::Duckerror::Io(format!("UC /tables/{table} parse: {e}")))?;
        cat.tables.insert(table.to_string(), t);
    }
    let t = cat
        .tables
        .get(table)
        .ok_or_else(|| types::Duckerror::Invalidargument(format!("table '{table}' not found")))?;
    if t.columns.is_empty() {
        return Err(types::Duckerror::Invalidargument(format!(
            "table '{table}' has no columns"
        )));
    }
    Ok(&t.columns)
}

/// Map a UC `type_text` (Databricks SQL type name) to a DuckDB logicaltype.
fn map_uc_type(type_text: &str) -> types::Logicaltype {
    // Strip any parameterization: "decimal(10,2)" -> "decimal".
    let base = type_text
        .trim()
        .split(['(', '<'])
        .next()
        .unwrap_or("")
        .trim()
        .to_ascii_lowercase();
    match base.as_str() {
        "boolean" | "bool" => types::Logicaltype::Boolean,
        "tinyint" | "byte" => types::Logicaltype::Int8,
        "smallint" | "short" => types::Logicaltype::Int16,
        "int" | "integer" => types::Logicaltype::Int32,
        "bigint" | "long" => types::Logicaltype::Int64,
        "float" | "real" => types::Logicaltype::Float32,
        "double" => types::Logicaltype::Float64,
        "date" => types::Logicaltype::Date,
        "timestamp" | "timestamp_ntz" => types::Logicaltype::Timestamp,
        "binary" => types::Logicaltype::Blob,
        // string / varchar / char / decimal / struct / array / map / interval
        // and anything unknown -> TEXT (UC reports the precise type in type_text;
        // DuckDB re-reads the actual data files via the delta/parquet readers).
        _ => types::Logicaltype::Text,
    }
}

export!(Extension);

// ---------------------------------------------------------------------------
// Native unit tests for the type mapping (REST/JSON parsing lives in uc.rs;
// the HTTP transport in http.rs). All offline -- no network.
// ---------------------------------------------------------------------------
#[cfg(test)]
mod tests {
    use super::*;

    // The generated `Logicaltype` variant does not derive PartialEq (the
    // `complex(string)` arm), so compare a small stable tag instead.
    fn tag(t: types::Logicaltype) -> &'static str {
        match t {
            types::Logicaltype::Boolean => "boolean",
            types::Logicaltype::Int8 => "int8",
            types::Logicaltype::Int16 => "int16",
            types::Logicaltype::Int32 => "int32",
            types::Logicaltype::Int64 => "int64",
            types::Logicaltype::Float32 => "float32",
            types::Logicaltype::Float64 => "float64",
            types::Logicaltype::Date => "date",
            types::Logicaltype::Timestamp => "timestamp",
            types::Logicaltype::Blob => "blob",
            types::Logicaltype::Text => "text",
            _ => "other",
        }
    }

    #[test]
    fn uc_type_mapping() {
        assert_eq!(tag(map_uc_type("boolean")), "boolean");
        assert_eq!(tag(map_uc_type("int")), "int32");
        assert_eq!(tag(map_uc_type("integer")), "int32");
        assert_eq!(tag(map_uc_type("bigint")), "int64");
        assert_eq!(tag(map_uc_type("double")), "float64");
        assert_eq!(tag(map_uc_type("float")), "float32");
        assert_eq!(tag(map_uc_type("timestamp")), "timestamp");
        assert_eq!(tag(map_uc_type("date")), "date");
        assert_eq!(tag(map_uc_type("binary")), "blob");
        // parameterized + unknown -> Text
        assert_eq!(tag(map_uc_type("decimal(10,2)")), "text");
        assert_eq!(tag(map_uc_type("varchar(255)")), "text");
        assert_eq!(tag(map_uc_type("string")), "text");
        assert_eq!(tag(map_uc_type("array<int>")), "text");
    }
}
