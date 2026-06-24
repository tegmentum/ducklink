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

wit_bindgen::generate!({ path: "./wit", world: "duckdb:extension/duckdb-extension-storage" });

use duckdb::extension::{storage, types};
use exports::duckdb::extension::{callback_dispatch, guest, storage_dispatch};

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
    fn call_scalar_batch(
        _h: u32,
        _r: Vec<Vec<types::Duckvalue>>,
        _c: types::Invokeinfo,
    ) -> Result<Vec<types::Duckvalue>, types::Duckerror> {
        Err(types::Duckerror::Unsupported("mysql: no scalar fns".into()))
    }
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
    fn call_aggregate(
        _h: u32,
        _r: types::Rowbatch,
    ) -> Result<types::Duckvalue, types::Duckerror> {
        Err(types::Duckerror::Unsupported("mysql: no aggs".into()))
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
/// round trip).
struct Catalog {
    conn: MyConn,
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
            Ok(cols
                .iter()
                .map(|c| types::Columndef {
                    name: c.name.clone().into(),
                    logical: coltype_to_logical(c.ty),
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
        // Dropping the Catalog drops the TcpStream, closing the connection.
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

/// Build + run the pushdown query, materializing rows mapped to logical types.
fn run_scan(
    cat: &mut Catalog,
    request: &storage::ScanRequest,
) -> Result<std::vec::Vec<std::vec::Vec<types::Duckvalue>>, types::Duckerror> {
    let cols: std::vec::Vec<Column> = fetch_columns(cat, &request.table)?.clone();

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
        proj.iter().map(|&i| backtick(&cols[i].name)).collect();
    let mut sql = format!(
        "SELECT {} FROM {}",
        select_list.join(", "),
        backtick(&request.table)
    );

    // WHERE: AND-join the filters, binding values inline (numbers raw, strings
    // single-quoted with '' escaping).
    let mut conds: std::vec::Vec<std::string::String> = std::vec::Vec::new();
    for fltr in &request.filters {
        let idx = fltr.column as usize;
        if idx >= cols.len() {
            return Err(types::Duckerror::Invalidargument(
                "filter column index out of range".into(),
            ));
        }
        let col = backtick(&cols[idx].name);
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
        for (slot, &ci) in proj.iter().enumerate() {
            let cell = row.get(slot).and_then(|c| c.as_ref());
            emit.push(text_to_duck(cell, cols[ci].ty));
        }
        out.push(emit);
    }
    Ok(out)
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
        types::Duckvalue::Int64(i) => i.to_string(),
        types::Duckvalue::Uint64(u) => u.to_string(),
        types::Duckvalue::Float64(f) => f.to_string(),
        types::Duckvalue::Text(s) => quote_str(s),
        types::Duckvalue::Blob(b) => quote_str(&String::from_utf8_lossy(b)),
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
