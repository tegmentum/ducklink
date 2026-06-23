//! Local web UI for the wasm DuckDB core.
//!
//! DuckDB's `ui` extension can't `listen()` inside the wasip2 sandbox (httplib's
//! accept loop hits the same select/poll gap that broke the httplib client).
//! Following the sqlite-wasm-httpd pattern, the NATIVE host owns the listening
//! socket + accept loop and bridges each request to the core component.
//!
//! Three modes (`ducklink ui`):
//! - `console` : a tiny built-in SQL console (embedded HTML, fully self-contained).
//! - `offline` : the REAL DuckDB UI SPA served from captured assets (web/duckdb-ui/),
//!               with /ddb/* bridged to the wasm core. No network.
//! - `online`  : the REAL DuckDB UI proxied live from ui.duckdb.org, with /ddb/*
//!               bridged to the wasm core.
//!
//! In the real-UI modes the genuine duckdb-ui C++ handlers run *inside* the
//! component (so /ddb/run emits DuckDB's exact BinarySerializer format) -- reached
//! via the `handle-ui-request` WIT export -> the `duckdb_ui_handle_request` C
//! bridge in the statically-linked ui extension.
//!
//! Single-threaded by design: localhost, one core instance + one connection held
//! in the accept loop.

use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use anyhow::{Context, Result};
use wasmtime::component::ResourceAny;

use super::duckdb_core_bindings::duckdb::extension::types as core_types;
use super::{
    build_engine, build_wasi_ctx_inherit, instantiate_core, ComponentArtifacts, CoreExecution,
    ExtensionManager,
};

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum UiMode {
    Console,
    Offline,
    Online,
}

const REMOTE_UI_URL: &str = "https://ui.duckdb.org";

/// Serve the UI on 127.0.0.1:`port`. Blocks forever.
pub fn serve_ui(
    artifacts: &ComponentArtifacts,
    db: Option<&str>,
    port: u16,
    mode: UiMode,
    open_browser: bool,
    assets_dir: &Path,
    preopen_refs: &[(&Path, &str)],
) -> Result<()> {
    let engine = build_engine()?;
    let wasi = build_wasi_ctx_inherit(&[String::from("duckdb-ui")], preopen_refs)?;
    let manager = Arc::new(Mutex::new(ExtensionManager::new(engine.clone())));
    let mut core = instantiate_core(&engine, &artifacts.core_component, wasi, manager)
        .context("failed to instantiate the core component")?;

    let db_arg = match db {
        None | Some(":memory:") | Some("") => None,
        Some(path) => Some(path.to_string()),
    };
    // DuckDB derives its extension-data dir from home_directory at open time; the
    // wasm default ("/") isn't writable in the sandbox. Set it (+ disable extension
    // autoinstall/autoload, since everything is statically linked) at open so the
    // open succeeds. "." resolves to the preopened cwd.
    let open_opts: Vec<(String, String)> = vec![
        ("autoinstall_known_extensions".to_string(), "false".to_string()),
        ("autoload_known_extensions".to_string(), "false".to_string()),
    ];
    let conn = core
        .with_database(|g, s| g.call_open_with_config(s, db_arg.as_deref(), &open_opts))?
        .map_err(|e| anyhow::anyhow!("open database: {e}"))?;

    if mode != UiMode::Console {
        // Initialize the ui extension's HttpServer singleton (bridge mode -- no
        // listen). The real-UI bridge needs it before handling /ddb/* requests.
        match core.with_database(|g, s| g.call_execute(s, conn.clone(), "SELECT * FROM start_ui_server()")) {
            Ok(Ok(_)) => {}
            other => eprintln!("duckdb-ui: start_ui_server() returned {other:?} (continuing)"),
        }
    }

    let listener = TcpListener::bind(("127.0.0.1", port))
        .with_context(|| format!("could not bind 127.0.0.1:{port}"))?;
    let url = format!("http://127.0.0.1:{port}/");
    let what = match mode {
        UiMode::Console => "SQL console",
        UiMode::Offline => "DuckDB UI (offline)",
        UiMode::Online => "DuckDB UI (online, proxied from ui.duckdb.org)",
    };
    eprintln!("duckdb-ui: serving the {what} at {url}");
    if open_browser {
        open_in_browser(&url);
    }

    let assets_dir = assets_dir.to_path_buf();
    for stream in listener.incoming() {
        let mut stream = match stream {
            Ok(s) => s,
            Err(e) => {
                eprintln!("duckdb-ui: accept error: {e}");
                continue;
            }
        };
        if let Err(e) = handle_connection(&mut stream, &mut core, &conn, mode, &assets_dir) {
            eprintln!("duckdb-ui: request error: {e}");
        }
    }
    Ok(())
}

fn open_in_browser(url: &str) {
    let opener = if cfg!(target_os = "macos") { "open" } else { "xdg-open" };
    let _ = std::process::Command::new(opener).arg(url).spawn();
}

struct Request {
    method: String,
    path: String,
    headers: String, // raw "Key: Value\n" block (forwarded to the bridge)
    body: Vec<u8>,
}

fn read_request(stream: &mut TcpStream) -> Result<Option<Request>> {
    let mut reader = BufReader::new(stream);
    let mut request_line = String::new();
    if reader.read_line(&mut request_line)? == 0 {
        return Ok(None);
    }
    let mut parts = request_line.split_whitespace();
    let method = parts.next().unwrap_or_default().to_string();
    let path = parts.next().unwrap_or_default().to_string();

    let mut headers = String::new();
    let mut content_length = 0usize;
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
            if k.eq_ignore_ascii_case("content-length") {
                content_length = v.trim().parse().unwrap_or(0);
            }
        }
        headers.push_str(trimmed);
        headers.push('\n');
    }

    let mut body = vec![0u8; content_length];
    if content_length > 0 {
        reader.read_exact(&mut body)?;
    }
    Ok(Some(Request { method, path, headers, body }))
}

/// Paths the duckdb-ui handlers own (everything else is an asset GET).
fn is_ui_endpoint(path: &str) -> bool {
    path == "/info"
        || path == "/localEvents"
        || path == "/localToken"
        || path.starts_with("/ddb/")
}

fn handle_connection(
    stream: &mut TcpStream,
    core: &mut CoreExecution,
    conn: &ResourceAny,
    mode: UiMode,
    assets_dir: &Path,
) -> Result<()> {
    let req = match read_request(stream)? {
        Some(r) => r,
        None => return Ok(()),
    };

    match mode {
        UiMode::Console => match (req.method.as_str(), req.path.as_str()) {
            ("GET", "/") => write_response(stream, 200, "text/html; charset=utf-8", CONSOLE_HTML.as_bytes()),
            ("POST", "/api/query") => {
                let json = run_query(core, conn, std::str::from_utf8(&req.body).unwrap_or("").trim());
                write_response(stream, 200, "application/json", json.as_bytes())
            }
            ("GET", "/favicon.ico") => write_response(stream, 204, "text/plain", b""),
            _ => write_response(stream, 404, "text/plain", b"not found"),
        },
        UiMode::Offline | UiMode::Online => {
            if is_ui_endpoint(&req.path) {
                match bridge_ui_request(core, conn, &req) {
                    Some((status, headers, body)) => {
                        write_response_with_headers(stream, status, &headers, &body)
                    }
                    None => write_response(stream, 503, "text/plain", b"UI server not started"),
                }
            } else if req.method == "GET" {
                let (status, ctype, body) = match mode {
                    UiMode::Offline => serve_asset(assets_dir, &req.path),
                    UiMode::Online => proxy_get(&req.path),
                    _ => unreachable!(),
                };
                write_response(stream, status, &ctype, &body)
            } else {
                write_response(stream, 404, "text/plain", b"not found")
            }
        }
    }
}

/// Forward a duckdb-ui request to the component's bridged HttpServer handler.
fn bridge_ui_request(
    core: &mut CoreExecution,
    _conn: &ResourceAny,
    req: &Request,
) -> Option<(u16, String, Vec<u8>)> {
    // returns (status, "Key: Value\n"-block of all response headers, body)
    let resp = core
        .with_database(|g, s| {
            g.call_handle_ui_request(s, &req.method, &req.path, &req.headers, &req.body)
        })
        .ok()??;
    Some((resp.status, resp.headers, resp.body))
}

/// Serve a captured asset from the offline assets directory.
fn serve_asset(assets_dir: &Path, path: &str) -> (u16, String, Vec<u8>) {
    let rel = path.split('?').next().unwrap_or("/").trim_start_matches('/');
    let rel = if rel.is_empty() { "index.html" } else { rel };
    // contain to assets_dir (no traversal)
    let mut full = PathBuf::from(assets_dir);
    for seg in rel.split('/') {
        if seg == ".." || seg == "." || seg.is_empty() {
            continue;
        }
        full.push(seg);
    }
    match std::fs::read(&full) {
        Ok(bytes) => (200, mime_for(&full).to_string(), bytes),
        Err(_) => {
            // SPA fallback: unknown non-asset path -> index.html (client routing)
            if !rel.contains('.') {
                if let Ok(bytes) = std::fs::read(assets_dir.join("index.html")) {
                    return (200, "text/html; charset=utf-8".to_string(), bytes);
                }
            }
            (404, "text/plain".to_string(), format!("not found: {path}").into_bytes())
        }
    }
}

/// Proxy a GET to ui.duckdb.org via curl (online mode; the host has outbound net).
fn proxy_get(path: &str) -> (u16, String, Vec<u8>) {
    let url = format!("{REMOTE_UI_URL}{}", if path == "/" { "/" } else { path });
    let hdr_file = std::env::temp_dir().join(format!("ddbui-{}", std::process::id()));
    let out = std::process::Command::new("curl")
        .args(["-s", "-A", "duckdb", "-D", hdr_file.to_str().unwrap_or("/dev/null"), &url])
        .output();
    match out {
        Ok(o) if o.status.success() => {
            let mut status = 200u16;
            let mut ctype = String::from("application/octet-stream");
            if let Ok(hdrs) = std::fs::read_to_string(&hdr_file) {
                for line in hdrs.lines() {
                    if let Some(code) = line.strip_prefix("HTTP/").and_then(|l| l.split_whitespace().nth(1)) {
                        status = code.parse().unwrap_or(status);
                    } else if let Some((k, v)) = line.split_once(':') {
                        if k.eq_ignore_ascii_case("content-type") {
                            ctype = v.trim().to_string();
                        }
                    }
                }
            }
            let _ = std::fs::remove_file(&hdr_file);
            (status, ctype, o.stdout)
        }
        _ => (502, "text/plain".to_string(), b"upstream fetch failed".to_vec()),
    }
}

fn mime_for(path: &Path) -> &'static str {
    match path.extension().and_then(|e| e.to_str()) {
        Some("html") => "text/html; charset=utf-8",
        Some("js") => "text/javascript; charset=utf-8",
        Some("css") => "text/css; charset=utf-8",
        Some("json") | Some("jsonl") => "application/json; charset=utf-8",
        Some("svg") => "image/svg+xml",
        Some("png") => "image/png",
        Some("woff2") => "font/woff2",
        Some("woff") => "font/woff",
        Some("wasm") => "application/wasm",
        Some("map") => "application/json",
        _ => "application/octet-stream",
    }
}

// --- console mode (the built-in tiny SQL console) ---------------------------

fn run_query(core: &mut CoreExecution, conn: &ResourceAny, sql: &str) -> String {
    if sql.is_empty() {
        return r#"{"columns":[],"rows":[],"rowcount":0}"#.to_string();
    }
    let result = match core.with_database(|g, s| g.call_execute(s, conn.clone(), sql)) {
        Ok(Ok(r)) => r,
        Ok(Err(e)) => return json_error(&duckerror_message(&e)),
        Err(e) => return json_error(&format!("{e}")),
    };
    let mut out = String::from("{\"columns\":[");
    for (i, col) in result.columns.iter().enumerate() {
        if i > 0 {
            out.push(',');
        }
        json_string(&mut out, &col.name);
    }
    out.push_str("],\"rows\":[");
    for (ri, row) in result.rows.iter().enumerate() {
        if ri > 0 {
            out.push(',');
        }
        out.push('[');
        for (ci, val) in row.iter().enumerate() {
            if ci > 0 {
                out.push(',');
            }
            json_value(&mut out, val);
        }
        out.push(']');
    }
    out.push_str("],\"rowcount\":");
    out.push_str(&result.rows.len().to_string());
    out.push('}');
    out
}

pub(crate) fn json_value(out: &mut String, val: &core_types::Duckvalue) {
    match val {
        core_types::Duckvalue::Null => out.push_str("null"),
        core_types::Duckvalue::Boolean(b) => out.push_str(if *b { "true" } else { "false" }),
        core_types::Duckvalue::Int64(v) => out.push_str(&v.to_string()),
        core_types::Duckvalue::Uint64(v) => out.push_str(&v.to_string()),
        core_types::Duckvalue::Float64(v) => {
            if v.is_finite() {
                out.push_str(&v.to_string());
            } else {
                json_string(out, &v.to_string());
            }
        }
        core_types::Duckvalue::Text(s) => json_string(out, s),
        core_types::Duckvalue::Blob(b) => json_string(out, &format!("\\x{} bytes", b.len())),
    }
}

pub(crate) fn duckerror_message(err: &core_types::Duckerror) -> String {
    match err {
        core_types::Duckerror::Invalidargument(m)
        | core_types::Duckerror::Unsupported(m)
        | core_types::Duckerror::Invalidstate(m)
        | core_types::Duckerror::Io(m)
        | core_types::Duckerror::Internal(m) => m.clone(),
    }
}

pub(crate) fn json_error(msg: &str) -> String {
    let mut out = String::from("{\"error\":");
    json_string(&mut out, msg);
    out.push('}');
    out
}

pub(crate) fn json_string(out: &mut String, s: &str) {
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
}

fn write_response(stream: &mut TcpStream, status: u16, content_type: &str, body: &[u8]) -> Result<()> {
    let reason = match status {
        200 => "OK",
        204 => "No Content",
        404 => "Not Found",
        502 => "Bad Gateway",
        503 => "Service Unavailable",
        _ => "OK",
    };
    let header = format!(
        "HTTP/1.1 {status} {reason}\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nConnection: close\r\nAccess-Control-Allow-Origin: *\r\n\r\n",
        body.len()
    );
    stream.write_all(header.as_bytes())?;
    stream.write_all(body)?;
    stream.flush()?;
    Ok(())
}

/// Write a response forwarding the full "Key: Value\n" header block the bridged
/// UI handler produced (carries the X-DuckDB-* version/metadata headers the SPA
/// reads). We compute Content-Length/Connection ourselves and drop any the
/// handler set to avoid conflicting duplicates.
fn write_response_with_headers(
    stream: &mut TcpStream,
    status: u16,
    headers: &str,
    body: &[u8],
) -> Result<()> {
    let reason = match status {
        200 => "OK",
        201 => "Created",
        204 => "No Content",
        400 => "Bad Request",
        404 => "Not Found",
        500 => "Internal Server Error",
        502 => "Bad Gateway",
        503 => "Service Unavailable",
        _ => "OK",
    };
    let mut out = format!("HTTP/1.1 {status} {reason}\r\n");
    for line in headers.lines() {
        let line = line.trim_end_matches('\r');
        if line.is_empty() {
            continue;
        }
        let name = line.split(':').next().unwrap_or("").trim();
        if name.eq_ignore_ascii_case("content-length")
            || name.eq_ignore_ascii_case("connection")
            || name.eq_ignore_ascii_case("transfer-encoding")
            || name.eq_ignore_ascii_case("access-control-allow-origin")
        {
            continue;
        }
        out.push_str(line);
        out.push_str("\r\n");
    }
    out.push_str(&format!(
        "Content-Length: {}\r\nConnection: close\r\nAccess-Control-Allow-Origin: *\r\n\r\n",
        body.len()
    ));
    stream.write_all(out.as_bytes())?;
    stream.write_all(body)?;
    stream.flush()?;
    Ok(())
}

const CONSOLE_HTML: &str = include_str!("ui_console.html");

// ---------------------------------------------------------------------------
// Tests — pure JSON rendering of DuckDB values (the wire format the SPA reads).
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn js(s: &str) -> String {
        let mut out = String::new();
        json_string(&mut out, s);
        out
    }

    fn jv(v: &core_types::Duckvalue) -> String {
        let mut out = String::new();
        json_value(&mut out, v);
        out
    }

    #[test]
    fn json_string_escapes_control_and_meta_chars() {
        assert_eq!(js("plain"), r#""plain""#);
        assert_eq!(js("a\"b"), r#""a\"b""#);
        assert_eq!(js("a\\b"), r#""a\\b""#);
        assert_eq!(js("a\nb\tc\rd"), r#""a\nb\tc\rd""#);
        // Other control chars below 0x20 use the \uXXXX form.
        assert_eq!(js("\u{0001}"), r#""\u0001""#);
        assert_eq!(js("\u{001f}"), r#""\u001f""#);
        // A printable above the control range is emitted verbatim (incl. unicode).
        assert_eq!(js("é"), "\"é\"");
    }

    #[test]
    fn json_value_per_variant() {
        assert_eq!(jv(&core_types::Duckvalue::Null), "null");
        assert_eq!(jv(&core_types::Duckvalue::Boolean(true)), "true");
        assert_eq!(jv(&core_types::Duckvalue::Boolean(false)), "false");
        assert_eq!(jv(&core_types::Duckvalue::Int64(-7)), "-7");
        assert_eq!(jv(&core_types::Duckvalue::Uint64(7)), "7");
        assert_eq!(jv(&core_types::Duckvalue::Text("x\"y".to_string())), r#""x\"y""#);
    }

    #[test]
    fn json_value_float_finite_vs_nonfinite() {
        assert_eq!(jv(&core_types::Duckvalue::Float64(1.5)), "1.5");
        // NaN/Infinity aren't valid JSON numbers, so they're emitted as strings.
        assert_eq!(jv(&core_types::Duckvalue::Float64(f64::NAN)), r#""NaN""#);
        assert_eq!(jv(&core_types::Duckvalue::Float64(f64::INFINITY)), r#""inf""#);
    }

    #[test]
    fn json_value_blob_is_summarized() {
        assert_eq!(
            jv(&core_types::Duckvalue::Blob(vec![1, 2, 3])),
            r#""\\x3 bytes""#
        );
    }

    #[test]
    fn duckerror_message_unwraps_every_variant() {
        assert_eq!(
            duckerror_message(&core_types::Duckerror::Invalidargument("bad".into())),
            "bad"
        );
        assert_eq!(
            duckerror_message(&core_types::Duckerror::Internal("boom".into())),
            "boom"
        );
    }

    #[test]
    fn json_error_wraps_message() {
        assert_eq!(json_error("oops"), r#"{"error":"oops"}"#);
        assert_eq!(json_error("a\"b"), r#"{"error":"a\"b"}"#);
    }
}
