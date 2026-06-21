//! duckdb-wasm-httpd — HTTP/HTTPS server that executes SQL against the wasm
//! DuckDB core component and returns JSON. A port of `sqlite-wasm-httpd`
//! (~/git/sqlite-wasm/sqlite-wasm-httpd) onto the DuckDB wasm core.
//!
//! Same contract as that sibling:
//!   - built-in admin surface: GET /health, GET|POST /sql, GET /tables,
//!     GET /schema/{name};
//!   - a database-driven router: an HTTP route is a row in a `routes` table
//!     mapping (method, GLOB pattern) -> a handler whose `kind` is
//!     `sql` | `static` | `blob` | `wasm`;
//!   - TLS (plain / self-signed / operator PEMs).
//!
//! Differences forced by the DuckDB substrate:
//!   - The core runs as a wasm component behind wasmtime; there is no native
//!     libduckdb in the hot path. All DB access crosses the component boundary
//!     via the `database` WIT interface, so this server is single-threaded
//!     (one core instance + one connection held in the accept loop, like the
//!     UI server). One request per connection (`Connection: close`).
//!   - Handler parameters are DuckDB **positional** `$1..$5`, bound safely via
//!     a prepared statement, in the fixed order:
//!       `$1`=method `$2`=path `$3`=query `$4`=body `$5`=remote.
//!     (sqlite-wasm-httpd uses `:name`; DuckDB's WIT execute is positional.)
//!   - `kind='wasm'` is reserved but not yet wired (DuckDB has no
//!     request-handler component world); it returns 501 with a clear message.

use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpListener;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use anyhow::{anyhow, Context, Result};
use wasmtime::component::ResourceAny;

use crate::duckdb_core_bindings::duckdb::extension::types as core_types;
use crate::handler::HandlerRegistry;
use crate::ui_server::{duckerror_message, json_string, json_value};
use crate::{
    build_engine, build_wasi_ctx_inherit, instantiate_core, ComponentArtifacts, CoreExecution,
    ExtensionManager,
};

use rustls::pki_types::{CertificateDer, PrivateKeyDer};
use rustls::{ServerConfig, ServerConnection, StreamOwned};

/// TLS configuration for the server. Mutually exclusive modes.
pub enum TlsMode {
    /// Plain HTTP.
    None,
    /// Generate a self-signed cert for `localhost` + the bind address.
    SelfSigned,
    /// Operator-supplied PEM cert + key.
    Files { cert: PathBuf, key: PathBuf },
}

/// Options for [`serve_httpd`].
pub struct HttpdOptions {
    /// Database path; `None`/`:memory:` keeps it in process memory.
    pub db: Option<String>,
    /// Bind address (e.g. `127.0.0.1`).
    pub bind: String,
    /// TCP port.
    pub port: u16,
    /// Routes table consulted for db-driven routing.
    pub routes_table: String,
    /// Create + seed the routes table on startup (idempotent).
    pub init_routes: bool,
    /// TLS configuration.
    pub tls: TlsMode,
}

/// Serve the httpd until the process is killed. Blocks forever.
///
/// `handlers` holds any `--load`ed request-handler components; routes with
/// `kind='wasm'` dispatch to them (None → such routes return 501).
pub fn serve_httpd(
    artifacts: &ComponentArtifacts,
    opts: &HttpdOptions,
    preopen_refs: &[(&std::path::Path, &str)],
    handlers: Option<HandlerRegistry>,
) -> Result<()> {
    let engine = build_engine()?;
    let wasi = build_wasi_ctx_inherit(&[String::from("duckdb-httpd")], preopen_refs)?;
    let manager = Arc::new(Mutex::new(ExtensionManager::new(engine.clone())));
    let mut core = instantiate_core(&engine, &artifacts.core_component, wasi, manager)
        .context("failed to instantiate the core component")?;

    let db_arg = match opts.db.as_deref() {
        None | Some(":memory:") | Some("") => None,
        Some(path) => Some(path.to_string()),
    };
    // Match the UI server: point extension-data at the (writable) cwd preopen
    // and disable extension autoinstall/autoload (everything is static).
    let open_opts: Vec<(String, String)> = vec![
        ("autoinstall_known_extensions".to_string(), "false".to_string()),
        ("autoload_known_extensions".to_string(), "false".to_string()),
    ];
    let conn = core
        .with_database(|g, s| g.call_open_with_config(s, db_arg.as_deref(), &open_opts))?
        .map_err(|e| anyhow!("open database: {e}"))?;

    if opts.init_routes {
        init_routes_table(&mut core, &conn, &opts.routes_table)
            .with_context(|| format!("init routes table {}", &opts.routes_table))?;
        eprintln!("duckdb-httpd: routes table `{}` ready", &opts.routes_table);
    }

    let tls = build_tls(&opts.tls)?;
    let listener = TcpListener::bind((opts.bind.as_str(), opts.port))
        .with_context(|| format!("could not bind {}:{}", opts.bind, opts.port))?;
    let scheme = if tls.is_some() { "https" } else { "http" };
    let db_label = opts.db.as_deref().unwrap_or(":memory:");
    eprintln!(
        "duckdb-httpd: {scheme}://{}:{}  db={db_label}  POST /sql | GET /sql?q=...",
        opts.bind, opts.port
    );

    for stream in listener.incoming() {
        let stream = match stream {
            Ok(s) => s,
            Err(e) => {
                eprintln!("duckdb-httpd: accept error: {e}");
                continue;
            }
        };
        let peer = stream
            .peer_addr()
            .map(|a| a.to_string())
            .unwrap_or_else(|_| "unknown".to_string());
        let result = match &tls {
            Some(cfg) => match ServerConnection::new(cfg.clone()) {
                Ok(sc) => {
                    let mut tls_stream = StreamOwned::new(sc, stream);
                    serve_conn(
                        &mut tls_stream,
                        &mut core,
                        &conn,
                        &opts.routes_table,
                        &peer,
                        handlers.as_ref(),
                    )
                }
                Err(e) => {
                    eprintln!("duckdb-httpd: tls setup: {e}");
                    continue;
                }
            },
            None => {
                let mut stream = stream;
                serve_conn(
                    &mut stream,
                    &mut core,
                    &conn,
                    &opts.routes_table,
                    &peer,
                    handlers.as_ref(),
                )
            }
        };
        if let Err(e) = result {
            eprintln!("duckdb-httpd: request error: {e}");
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// HTTP plumbing (generic over the stream so plain TCP and rustls share a path)
// ---------------------------------------------------------------------------

struct Request {
    method: String,
    path: String,
    query: Option<String>,
    headers: Vec<(String, String)>,
    body: Vec<u8>,
}

fn read_request<S: Read>(stream: &mut S) -> Result<Option<Request>> {
    let mut reader = BufReader::new(stream);
    let mut request_line = String::new();
    if reader.read_line(&mut request_line)? == 0 {
        return Ok(None);
    }
    let mut parts = request_line.split_whitespace();
    let method = parts.next().unwrap_or_default().to_string();
    let target = parts.next().unwrap_or_default().to_string();
    let (path, query) = match target.split_once('?') {
        Some((p, q)) => (p.to_string(), Some(q.to_string())),
        None => (target, None),
    };

    let mut content_length = 0usize;
    let mut headers: Vec<(String, String)> = Vec::new();
    loop {
        let mut line = String::new();
        if reader.read_line(&mut line)? == 0 {
            break;
        }
        let trimmed = line.trim_end();
        if trimmed.is_empty() {
            break;
        }
        if let Some((k, v)) = trimmed.split_once(':') {
            let key = k.trim();
            let val = v.trim();
            if key.eq_ignore_ascii_case("content-length") {
                content_length = val.parse().unwrap_or(0);
            }
            headers.push((key.to_ascii_lowercase(), val.to_string()));
        }
    }

    let mut body = vec![0u8; content_length];
    if content_length > 0 {
        reader.read_exact(&mut body)?;
    }
    Ok(Some(Request {
        method,
        path,
        query,
        headers,
        body,
    }))
}

fn serve_conn<S: Read + Write>(
    stream: &mut S,
    core: &mut CoreExecution,
    conn: &ResourceAny,
    routes_table: &str,
    peer: &str,
    handlers: Option<&HandlerRegistry>,
) -> Result<()> {
    let req = match read_request(stream)? {
        Some(r) => r,
        None => return Ok(()),
    };
    let resp = handle(core, conn, &req, routes_table, peer, handlers);
    write_response(stream, resp.status, &resp.ctype, &resp.body)
}

struct HttpResponse {
    status: u16,
    ctype: String,
    body: Vec<u8>,
}

impl HttpResponse {
    fn new(status: u16, ctype: &str, body: Vec<u8>) -> Self {
        HttpResponse {
            status,
            ctype: ctype.to_string(),
            body,
        }
    }
    fn text(status: u16, body: &str) -> Self {
        Self::new(status, "text/plain; charset=utf-8", body.as_bytes().to_vec())
    }
    fn json(status: u16, body: String) -> Self {
        Self::new(status, "application/json", body.into_bytes())
    }
    fn error(status: u16, msg: &str) -> Self {
        let mut out = String::from("{\"error\":");
        json_string(&mut out, msg);
        out.push('}');
        Self::json(status, out)
    }
}

fn write_response<S: Write>(stream: &mut S, status: u16, ctype: &str, body: &[u8]) -> Result<()> {
    let reason = reason_phrase(status);
    let header = format!(
        "HTTP/1.1 {status} {reason}\r\nContent-Type: {ctype}\r\nContent-Length: {}\r\nConnection: close\r\nAccess-Control-Allow-Origin: *\r\n\r\n",
        body.len()
    );
    stream.write_all(header.as_bytes())?;
    stream.write_all(body)?;
    stream.flush()?;
    Ok(())
}

fn reason_phrase(status: u16) -> &'static str {
    match status {
        200 => "OK",
        201 => "Created",
        204 => "No Content",
        400 => "Bad Request",
        404 => "Not Found",
        422 => "Unprocessable Entity",
        500 => "Internal Server Error",
        501 => "Not Implemented",
        _ => "OK",
    }
}

// ---------------------------------------------------------------------------
// Routing
// ---------------------------------------------------------------------------

fn handle(
    core: &mut CoreExecution,
    conn: &ResourceAny,
    req: &Request,
    routes_table: &str,
    peer: &str,
    handlers: Option<&HandlerRegistry>,
) -> HttpResponse {
    let method = req.method.as_str();
    let path = req.path.as_str();

    // Built-in admin endpoints take precedence over db-driven routes.
    let builtin = matches!(
        (method, path),
        ("GET", "/health") | ("GET", "/sql") | ("POST", "/sql") | ("GET", "/tables")
    ) || path.starts_with("/schema/");

    if !builtin {
        match lookup(core, conn, method, path, routes_table) {
            Ok(Some(m)) => return execute_route(core, conn, &m, req, peer, handlers),
            Ok(None) => {}
            Err(e) => {
                // Routes table missing / malformed → fall through to 404.
                eprintln!("duckdb-httpd: router lookup: {e}");
            }
        }
    }

    match (method, path) {
        ("GET", "/health") => HttpResponse::text(200, "ok"),
        ("GET", "/tables") => {
            let sql = "SELECT table_name FROM information_schema.tables \
                       WHERE table_schema = 'main' ORDER BY table_name";
            run_sql(core, conn, sql)
        }
        ("GET", p) if p.starts_with("/schema/") => {
            let name = &p["/schema/".len()..];
            if name.is_empty() || !name.chars().all(|c| c.is_alphanumeric() || c == '_') {
                return HttpResponse::error(400, "bad table name");
            }
            let sql = format!("SELECT * FROM pragma_table_info('{name}')");
            run_sql(core, conn, &sql)
        }
        ("POST", "/sql") => match std::str::from_utf8(&req.body) {
            Ok(sql) => run_sql(core, conn, sql.trim()),
            Err(_) => HttpResponse::error(400, "body not UTF-8"),
        },
        ("GET", "/sql") => match req.query.as_deref().and_then(parse_q) {
            Some(sql) => run_sql(core, conn, &sql),
            None => HttpResponse::error(400, "missing q parameter"),
        },
        _ => HttpResponse::error(404, "no such route"),
    }
}

/// Decode the `q=` parameter from a raw query string (percent-decoded).
fn parse_q(query: &str) -> Option<String> {
    for pair in query.split('&') {
        let mut it = pair.splitn(2, '=');
        let k = it.next()?;
        if k != "q" {
            continue;
        }
        let v = it.next()?;
        return Some(percent_decode(v));
    }
    None
}

/// Minimal application/x-www-form-urlencoded percent-decoder (`+` → space).
fn percent_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'%' if i + 2 < bytes.len() => {
                let hi = (bytes[i + 1] as char).to_digit(16);
                let lo = (bytes[i + 2] as char).to_digit(16);
                if let (Some(h), Some(l)) = (hi, lo) {
                    out.push((h * 16 + l) as u8);
                    i += 3;
                    continue;
                }
                out.push(b'%');
                i += 1;
            }
            b'+' => {
                out.push(b' ');
                i += 1;
            }
            c => {
                out.push(c);
                i += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// Run admin SQL and emit `{columns, rows, rowcount}` (200) or `{error}` (422).
fn run_sql(core: &mut CoreExecution, conn: &ResourceAny, sql: &str) -> HttpResponse {
    if sql.is_empty() {
        return HttpResponse::json(200, r#"{"columns":[],"rows":[],"rowcount":0}"#.to_string());
    }
    match db_query(core, conn, sql) {
        Ok(rows) => HttpResponse::json(200, rows_to_result_json(&rows)),
        Err(e) => HttpResponse::error(422, &e),
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum RouteKind {
    Sql,
    Static,
    Wasm,
    Blob,
}

impl RouteKind {
    fn parse(s: &str) -> Self {
        match s.to_ascii_lowercase().as_str() {
            "static" => Self::Static,
            "wasm" => Self::Wasm,
            "blob" => Self::Blob,
            _ => Self::Sql,
        }
    }
}

struct RouteMatch {
    kind: RouteKind,
    handler: String,
    status: i64,
    ctype: Option<String>,
}

fn is_safe_ident(s: &str) -> bool {
    !s.is_empty() && s.chars().all(|c| c.is_alphanumeric() || c == '_')
}

/// Look up the best-matching route. `None` → fall through to built-ins / 404.
fn lookup(
    core: &mut CoreExecution,
    conn: &ResourceAny,
    method: &str,
    path: &str,
    table: &str,
) -> Result<Option<RouteMatch>, String> {
    if !is_safe_ident(table) {
        return Err("bad routes table name".to_string());
    }
    let params = [
        core_types::Duckvalue::Text(method.to_string()),
        core_types::Duckvalue::Text(path.to_string()),
    ];
    let sql = format!(
        "SELECT handler, COALESCE(status, 200), ctype, COALESCE(kind, 'sql') FROM {table} \
         WHERE (method = $1 OR method = '*') AND $2 GLOB pattern \
         ORDER BY priority DESC, length(pattern) DESC LIMIT 1"
    );
    let rows = match db_query_params(core, conn, &sql, &params) {
        Ok(r) => r,
        Err(e) if e.contains("kind") => {
            // Legacy routes table without the `kind` column.
            let legacy = format!(
                "SELECT handler, COALESCE(status, 200), ctype, 'sql' FROM {table} \
                 WHERE (method = $1 OR method = '*') AND $2 GLOB pattern \
                 ORDER BY priority DESC, length(pattern) DESC LIMIT 1"
            );
            db_query_params(core, conn, &legacy, &params)?
        }
        Err(e) => return Err(e),
    };
    let Some(row) = rows.rows.into_iter().next() else {
        return Ok(None);
    };
    let handler = row.first().and_then(dv_as_str).unwrap_or_default().to_string();
    if handler.is_empty() {
        return Ok(None);
    }
    let status = row.get(1).and_then(dv_as_i64).unwrap_or(200);
    let ctype = row.get(2).and_then(dv_as_str).map(|s| s.to_string());
    let kind = row
        .get(3)
        .and_then(dv_as_str)
        .map(RouteKind::parse)
        .unwrap_or(RouteKind::Sql);
    Ok(Some(RouteMatch {
        kind,
        handler,
        status,
        ctype,
    }))
}

fn execute_route(
    core: &mut CoreExecution,
    conn: &ResourceAny,
    m: &RouteMatch,
    req: &Request,
    peer: &str,
    handlers: Option<&HandlerRegistry>,
) -> HttpResponse {
    match m.kind {
        RouteKind::Static => {
            let ctype = m.ctype.as_deref().unwrap_or("text/plain; charset=utf-8");
            HttpResponse::new(clamp_status(m.status), ctype, m.handler.clone().into_bytes())
        }
        RouteKind::Wasm => execute_wasm(m, req, peer, handlers),
        RouteKind::Sql => execute_sql(core, conn, m, req, peer),
        RouteKind::Blob => execute_blob(core, conn, m, req, peer),
    }
}

/// Dispatch a `kind='wasm'` route to a loaded request-handler component. The
/// request is serialized to JSON (the `duckdb:handler` contract); the handler's
/// return string is either a raw body or a `{status, body, ctype}` object.
fn execute_wasm(
    m: &RouteMatch,
    req: &Request,
    peer: &str,
    handlers: Option<&HandlerRegistry>,
) -> HttpResponse {
    let Some(registry) = handlers else {
        return HttpResponse::error(
            501,
            "wasm route hit but no handlers loaded (pass --load NAME=PATH)",
        );
    };
    let request_json = build_request_json(req, peer);
    match registry.invoke(&m.handler, &request_json) {
        Ok(Ok(body)) => parse_handler_response(m, body),
        Ok(Err(e)) => HttpResponse::error(500, &format!("handler `{}`: {e}", m.handler)),
        Err(e) => HttpResponse::error(500, &format!("handler `{}` dispatch: {e}", m.handler)),
    }
}

/// Serialize the request as the JSON blob handler components receive.
fn build_request_json(req: &Request, peer: &str) -> String {
    let mut out = String::from("{\"method\":");
    json_string(&mut out, &req.method);
    out.push_str(",\"path\":");
    json_string(&mut out, &req.path);
    out.push_str(",\"query\":");
    match &req.query {
        Some(q) => json_string(&mut out, q),
        None => out.push_str("null"),
    }
    out.push_str(",\"remote\":");
    json_string(&mut out, peer);
    out.push_str(",\"headers\":{");
    for (i, (k, v)) in req.headers.iter().enumerate() {
        if i > 0 {
            out.push(',');
        }
        json_string(&mut out, k);
        out.push(':');
        json_string(&mut out, v);
    }
    out.push_str("},\"body\":");
    match std::str::from_utf8(&req.body) {
        Ok(s) => {
            out.push_str("{\"text\":");
            json_string(&mut out, s);
            out.push('}');
        }
        Err(_) => {
            out.push_str("{\"bytes_hex\":\"");
            for b in &req.body {
                out.push_str(&format!("{b:02x}"));
            }
            out.push_str("\"}");
        }
    }
    out.push('}');
    out
}

/// Interpret a handler's return string. If it parses as a JSON object with
/// `status`/`body`/`ctype`, apply those; otherwise the raw string is the body.
fn parse_handler_response(m: &RouteMatch, body: String) -> HttpResponse {
    if let Some((status, ctype, body_bytes)) = parse_structured_response(&body) {
        let status = status.unwrap_or_else(|| clamp_status(m.status));
        let ctype = ctype
            .or_else(|| m.ctype.clone())
            .unwrap_or_else(|| "application/json".to_string());
        return HttpResponse::new(status, &ctype, body_bytes);
    }
    let ctype = m.ctype.as_deref().unwrap_or("application/json");
    HttpResponse::new(clamp_status(m.status), ctype, body.into_bytes())
}

/// Parse a `{ "status": N, "ctype": "...", "body": "..." }` object. Returns None
/// if `s` isn't an object with at least one of those keys (so the caller falls
/// back to treating the whole string as the body). A string `body` is emitted
/// verbatim; any other JSON `body` is re-serialized.
fn parse_structured_response(s: &str) -> Option<(Option<u16>, Option<String>, Vec<u8>)> {
    let v: serde_json::Value = serde_json::from_str(s).ok()?;
    let obj = v.as_object()?;
    if !(obj.contains_key("status") || obj.contains_key("body") || obj.contains_key("ctype")) {
        return None;
    }
    let status = obj.get("status").and_then(|v| v.as_i64()).map(clamp_status);
    let ctype = obj
        .get("ctype")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string());
    let body = match obj.get("body") {
        Some(serde_json::Value::String(s)) => s.clone().into_bytes(),
        Some(serde_json::Value::Null) | None => Vec::new(),
        Some(other) => other.to_string().into_bytes(),
    };
    Some((status, ctype, body))
}

/// Resolve the handler's DuckDB **named** parameters to bound values.
///
/// Handlers reference the request via `$method`, `$path`, `$query`, `$body`,
/// `$remote` (any subset, any order). DuckDB numbers named parameters by order
/// of first appearance, and the WIT `execute` binds positionally — so we scan
/// the handler text for those names and return the values in first-appearance
/// order, matching how DuckDB will index them.
///
/// Limitation: the scan doesn't parse SQL, so one of these tokens appearing
/// inside a string literal or `$tag$` dollar-quote would be miscounted. Operator
/// SQL rarely does that; a future revision can add named binding to the WIT.
fn ordered_handler_params(sql: &str, req: &Request, peer: &str) -> Vec<core_types::Duckvalue> {
    let body_text = std::str::from_utf8(&req.body).ok().map(|s| s.to_string());
    let known: [(&str, core_types::Duckvalue); 5] = [
        ("method", core_types::Duckvalue::Text(req.method.clone())),
        ("path", core_types::Duckvalue::Text(req.path.clone())),
        ("query", opt_text(req.query.clone())),
        ("body", opt_text(body_text)),
        ("remote", core_types::Duckvalue::Text(peer.to_string())),
    ];
    let bytes = sql.as_bytes();
    let mut seen = [false; 5];
    let mut ordered: Vec<core_types::Duckvalue> = Vec::new();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'$' {
            let start = i + 1;
            let mut j = start;
            while j < bytes.len() && (bytes[j].is_ascii_alphanumeric() || bytes[j] == b'_') {
                j += 1;
            }
            let name = &sql[start..j];
            for (k, (pname, val)) in known.iter().enumerate() {
                if !seen[k] && name == *pname {
                    seen[k] = true;
                    ordered.push(val.clone());
                }
            }
            i = j.max(start);
        } else {
            i += 1;
        }
    }
    ordered
}

fn execute_sql(
    core: &mut CoreExecution,
    conn: &ResourceAny,
    m: &RouteMatch,
    req: &Request,
    peer: &str,
) -> HttpResponse {
    let params = ordered_handler_params(&m.handler, req, peer);
    match db_query_params(core, conn, &m.handler, &params) {
        Ok(rows) => build_route_response(m, rows),
        Err(e) => HttpResponse::error(500, &e),
    }
}

fn execute_blob(
    core: &mut CoreExecution,
    conn: &ResourceAny,
    m: &RouteMatch,
    req: &Request,
    peer: &str,
) -> HttpResponse {
    let params = ordered_handler_params(&m.handler, req, peer);
    let rows = match db_query_params(core, conn, &m.handler, &params) {
        Ok(r) => r,
        Err(e) => return HttpResponse::error(500, &e),
    };
    let Some(first) = rows.rows.into_iter().next() else {
        return HttpResponse::text(404, "not found");
    };
    let bytes = first
        .into_iter()
        .next()
        .map(dv_to_body_bytes)
        .unwrap_or_default();
    let ctype = m.ctype.as_deref().unwrap_or("application/octet-stream");
    HttpResponse::new(clamp_status(m.status), ctype, bytes)
}

/// Interpret a SQL handler's result into a response (mirrors the sqlite port):
///   0 rows → 204; 1 row with body/status/ctype columns → structured; 1 row /
///   1 col → that value IS the body; 1 row / N cols → JSON object; N rows →
///   JSON array of objects.
fn build_route_response(m: &RouteMatch, rows: Rows) -> HttpResponse {
    let default_status = clamp_status(m.status);
    let default_ctype = m.ctype.clone();

    if rows.rows.is_empty() {
        return HttpResponse::new(
            204,
            default_ctype.as_deref().unwrap_or("application/json"),
            Vec::new(),
        );
    }

    if rows.rows.len() == 1 {
        let row = &rows.rows[0];
        let col_idx = |name: &str| rows.cols.iter().position(|c| c == name);
        let body_idx = col_idx("body");
        let status_idx = col_idx("status");
        let ctype_idx = col_idx("ctype").or_else(|| col_idx("content_type"));
        if body_idx.is_some() || status_idx.is_some() || ctype_idx.is_some() {
            let status = status_idx
                .and_then(|i| row.get(i))
                .and_then(dv_as_i64)
                .map(|s| clamp_status(s))
                .unwrap_or(default_status);
            let ctype = ctype_idx
                .and_then(|i| row.get(i))
                .and_then(dv_as_str)
                .map(|s| s.to_string())
                .or(default_ctype.clone())
                .unwrap_or_else(|| "application/json".to_string());
            let body = body_idx
                .and_then(|i| row.get(i))
                .cloned()
                .map(dv_to_body_bytes)
                .unwrap_or_default();
            return HttpResponse::new(status, &ctype, body);
        }
        if rows.cols.len() == 1 {
            let body = dv_to_body_bytes(row[0].clone());
            return HttpResponse::new(
                default_status,
                default_ctype.as_deref().unwrap_or("application/json"),
                body,
            );
        }
        let obj = row_to_json_object(&rows.cols, row);
        return HttpResponse::new(
            default_status,
            default_ctype.as_deref().unwrap_or("application/json"),
            obj.into_bytes(),
        );
    }

    // Multi-row → JSON array of objects.
    let mut out = String::from("[");
    for (ri, row) in rows.rows.iter().enumerate() {
        if ri > 0 {
            out.push(',');
        }
        out.push_str(&row_to_json_object(&rows.cols, row));
    }
    out.push(']');
    HttpResponse::new(
        default_status,
        default_ctype.as_deref().unwrap_or("application/json"),
        out.into_bytes(),
    )
}

// ---------------------------------------------------------------------------
// DB access over the component boundary
// ---------------------------------------------------------------------------

struct Rows {
    cols: Vec<String>,
    rows: Vec<Vec<core_types::Duckvalue>>,
}

/// Run SQL with no parameters via `execute`.
fn db_query(core: &mut CoreExecution, conn: &ResourceAny, sql: &str) -> Result<Rows, String> {
    match core.with_database(|g, s| g.call_execute(s, conn.clone(), sql)) {
        Ok(Ok(r)) => Ok(Rows {
            cols: r.columns.into_iter().map(|c| c.name).collect(),
            rows: r.rows,
        }),
        Ok(Err(e)) => Err(duckerror_message(&e)),
        Err(e) => Err(e.to_string()),
    }
}

/// Run SQL with positional parameters (`$1..$n`) via a prepared statement.
/// Binds exactly `parameter_count` values from `params`, padding with NULL —
/// so a handler referencing `$4` binds [method, path, query, body] and a
/// 2-param lookup binds [method, path].
fn db_query_params(
    core: &mut CoreExecution,
    conn: &ResourceAny,
    sql: &str,
    params: &[core_types::Duckvalue],
) -> Result<Rows, String> {
    let prepared: ResourceAny = match core.with_database(|g, s| g.call_prepare(s, conn.clone(), sql))
    {
        Ok(Ok(p)) => p,
        Ok(Err(e)) => return Err(duckerror_message(&e)),
        Err(e) => return Err(e.to_string()),
    };

    let count = core
        .with_prepared(|g, s| g.call_parameter_count(s, prepared))
        .map_err(|e| e.to_string())? as usize;
    let bound: Vec<core_types::Duckvalue> = (0..count)
        .map(|i| params.get(i).cloned().unwrap_or(core_types::Duckvalue::Null))
        .collect();

    let result = core.with_prepared(|g, s| g.call_execute(s, prepared, &bound));
    // Free the prepared-statement resource regardless of outcome.
    let _ = core.with_prepared(|_g, s| prepared.resource_drop(s));

    match result {
        Ok(Ok(r)) => Ok(Rows {
            cols: r.columns.into_iter().map(|c| c.name).collect(),
            rows: r.rows,
        }),
        Ok(Err(e)) => Err(duckerror_message(&e)),
        Err(e) => Err(e.to_string()),
    }
}

// ---------------------------------------------------------------------------
// Value helpers
// ---------------------------------------------------------------------------

fn opt_text(v: Option<String>) -> core_types::Duckvalue {
    match v {
        Some(s) => core_types::Duckvalue::Text(s),
        None => core_types::Duckvalue::Null,
    }
}

fn dv_as_str(v: &core_types::Duckvalue) -> Option<&str> {
    match v {
        core_types::Duckvalue::Text(s) => Some(s.as_str()),
        _ => None,
    }
}

fn dv_as_i64(v: &core_types::Duckvalue) -> Option<i64> {
    match v {
        core_types::Duckvalue::Int64(i) => Some(*i),
        core_types::Duckvalue::Uint64(u) => Some(*u as i64),
        _ => None,
    }
}

/// Convert a value into raw response-body bytes (text/blob stay verbatim;
/// scalars stringify; NULL → empty).
fn dv_to_body_bytes(v: core_types::Duckvalue) -> Vec<u8> {
    match v {
        core_types::Duckvalue::Null => Vec::new(),
        core_types::Duckvalue::Boolean(b) => if b { "true" } else { "false" }.as_bytes().to_vec(),
        core_types::Duckvalue::Int64(i) => i.to_string().into_bytes(),
        core_types::Duckvalue::Uint64(u) => u.to_string().into_bytes(),
        core_types::Duckvalue::Float64(f) => f.to_string().into_bytes(),
        core_types::Duckvalue::Text(s) => s.into_bytes(),
        core_types::Duckvalue::Blob(b) => b,
    }
}

fn row_to_json_object(cols: &[String], row: &[core_types::Duckvalue]) -> String {
    let mut out = String::from("{");
    for (i, c) in cols.iter().enumerate() {
        if i > 0 {
            out.push(',');
        }
        json_string(&mut out, c);
        out.push(':');
        match row.get(i) {
            Some(v) => json_value(&mut out, v),
            None => out.push_str("null"),
        }
    }
    out.push('}');
    out
}

fn rows_to_result_json(rows: &Rows) -> String {
    let mut out = String::from("{\"columns\":[");
    for (i, c) in rows.cols.iter().enumerate() {
        if i > 0 {
            out.push(',');
        }
        json_string(&mut out, c);
    }
    out.push_str("],\"rows\":[");
    for (ri, row) in rows.rows.iter().enumerate() {
        if ri > 0 {
            out.push(',');
        }
        out.push('[');
        for (ci, v) in row.iter().enumerate() {
            if ci > 0 {
                out.push(',');
            }
            json_value(&mut out, v);
        }
        out.push(']');
    }
    out.push_str("],\"rowcount\":");
    out.push_str(&rows.rows.len().to_string());
    out.push('}');
    out
}

fn clamp_status(s: i64) -> u16 {
    if (100..=599).contains(&s) {
        s as u16
    } else {
        200
    }
}

// ---------------------------------------------------------------------------
// Routes-table bootstrap
// ---------------------------------------------------------------------------

fn init_routes_table(core: &mut CoreExecution, conn: &ResourceAny, table: &str) -> Result<()> {
    if !is_safe_ident(table) {
        anyhow::bail!("bad routes table name");
    }
    let ddl = format!(
        "CREATE TABLE IF NOT EXISTS {table} (
            method   VARCHAR NOT NULL,
            pattern  VARCHAR NOT NULL,
            handler  VARCHAR NOT NULL,
            kind     VARCHAR NOT NULL DEFAULT 'sql',
            status   INTEGER DEFAULT 200,
            ctype    VARCHAR,
            priority INTEGER DEFAULT 0
         )"
    );
    db_query(core, conn, &ddl).map_err(|e| anyhow!("{e}"))?;

    let count_rows = db_query(core, conn, &format!("SELECT count(*) FROM {table}"))
        .map_err(|e| anyhow!("{e}"))?;
    let count = count_rows
        .rows
        .first()
        .and_then(|r| r.first())
        .and_then(dv_as_i64)
        .unwrap_or(0);
    if count == 0 {
        let seed = format!(
            "INSERT INTO {table} (method, pattern, handler, kind, ctype) VALUES \
             ('GET', '/hello', 'SELECT ''{{}}'' AS body', 'sql', 'application/json')"
        );
        db_query(core, conn, &seed).map_err(|e| anyhow!("{e}"))?;
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// TLS
// ---------------------------------------------------------------------------

fn build_tls(mode: &TlsMode) -> Result<Option<Arc<ServerConfig>>> {
    match mode {
        TlsMode::None => Ok(None),
        TlsMode::SelfSigned => {
            install_crypto_provider();
            let cert = rcgen::generate_simple_self_signed(vec!["localhost".to_string()])
                .map_err(|e| anyhow!("rcgen: {e}"))?;
            let cert_der = CertificateDer::from(cert.cert.der().to_vec());
            let key_der = PrivateKeyDer::try_from(cert.key_pair.serialize_der())
                .map_err(|e| anyhow!("self-signed key: {e}"))?;
            let cfg = ServerConfig::builder()
                .with_no_client_auth()
                .with_single_cert(vec![cert_der], key_der)
                .map_err(|e| anyhow!("rustls: {e}"))?;
            Ok(Some(Arc::new(cfg)))
        }
        TlsMode::Files { cert, key } => {
            install_crypto_provider();
            let cert_bytes = std::fs::read(cert)
                .with_context(|| format!("read tls cert {}", cert.display()))?;
            let key_bytes =
                std::fs::read(key).with_context(|| format!("read tls key {}", key.display()))?;
            let certs: Vec<CertificateDer<'static>> =
                rustls_pemfile::certs(&mut &cert_bytes[..])
                    .collect::<std::result::Result<_, _>>()
                    .map_err(|e| anyhow!("parse cert PEM: {e}"))?;
            if certs.is_empty() {
                anyhow::bail!("no certificates found in {}", cert.display());
            }
            let key_der = rustls_pemfile::private_key(&mut &key_bytes[..])
                .map_err(|e| anyhow!("parse key PEM: {e}"))?
                .ok_or_else(|| anyhow!("no private key found in {}", key.display()))?;
            let cfg = ServerConfig::builder()
                .with_no_client_auth()
                .with_single_cert(certs, key_der)
                .map_err(|e| anyhow!("rustls: {e}"))?;
            Ok(Some(Arc::new(cfg)))
        }
    }
}

fn install_crypto_provider() {
    // Idempotent: ignore the error if a provider is already installed.
    let _ = rustls::crypto::ring::default_provider().install_default();
}
