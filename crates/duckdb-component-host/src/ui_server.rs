//! A minimal local web UI for the wasm DuckDB core.
//!
//! DuckDB's own `ui` extension can't run inside the wasip2 sandbox (it embeds an
//! httplib server that `listen()`s, and httplib's accept loop hits the same
//! select/poll gap that broke the httplib *client*). Following the
//! sqlite-wasm-httpd pattern, the NATIVE host owns the listening socket + accept
//! loop and bridges each request to the core component, which actually runs the
//! SQL. This is our own equivalent: a small SQL console served over HTTP whose
//! `POST /api/query` executes against the core (via the same `call_execute` path
//! the CLI/tests use) and returns JSON.
//!
//! Single-threaded by design: localhost, single user, one core instance + one
//! connection held in the accept loop -- no Send/Sync gymnastics around the
//! wasmtime Store.

use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::Path;
use std::sync::{Arc, Mutex};

use anyhow::{Context, Result};
use wasmtime::component::ResourceAny;

use super::duckdb_core_bindings::duckdb::extension::types as core_types;
use super::{
    build_engine, build_wasi_ctx_inherit, instantiate_core, ComponentArtifacts, CoreExecution,
    ExtensionManager,
};

/// Serve the SQL console on 127.0.0.1:`port`, executing against a fresh core
/// instance (in-memory unless `db` is a reachable path). Blocks forever.
pub fn serve_ui(
    artifacts: &ComponentArtifacts,
    db: Option<&str>,
    port: u16,
    open_browser: bool,
    preopen_refs: &[(&Path, &str)],
) -> Result<()> {
    let engine = build_engine()?;
    let wasi = build_wasi_ctx_inherit(&[String::from("duckdb-ui")], preopen_refs)?;
    let manager = Arc::new(Mutex::new(ExtensionManager::new(engine.clone())));
    let mut core = instantiate_core(&engine, &artifacts.core_component, wasi, manager)
        .context("failed to instantiate the core component")?;

    // `db` of None / ":memory:" -> in-memory; a path must be inside a preopen.
    let db_arg = match db {
        None | Some(":memory:") | Some("") => None,
        Some(path) => Some(path.to_string()),
    };
    let conn = core
        .with_database(|g, s| g.call_open(s, db_arg.as_deref()))?
        .map_err(|e| anyhow::anyhow!("open database: {e:?}"))?;

    // DuckDB's home-directory detection fails on wasm (no $HOME path it can
    // resolve), which breaks duckdb_extensions() etc. Point it at the preopened
    // cwd so introspection + extension settings work. Best-effort.
    let _ = core.with_database(|g, s| g.call_execute(s, conn.clone(), "SET home_directory='.'"));

    let listener = TcpListener::bind(("127.0.0.1", port))
        .with_context(|| format!("could not bind 127.0.0.1:{port}"))?;
    let url = format!("http://127.0.0.1:{port}/");
    eprintln!("duckdb-ui: serving the SQL console at {url}");
    if open_browser {
        open_in_browser(&url);
    }

    for stream in listener.incoming() {
        let mut stream = match stream {
            Ok(s) => s,
            Err(e) => {
                eprintln!("duckdb-ui: accept error: {e}");
                continue;
            }
        };
        if let Err(e) = handle_connection(&mut stream, &mut core, &conn) {
            eprintln!("duckdb-ui: request error: {e}");
        }
    }
    Ok(())
}

fn open_in_browser(url: &str) {
    // best-effort; macOS `open`, Linux `xdg-open`
    let opener = if cfg!(target_os = "macos") {
        "open"
    } else {
        "xdg-open"
    };
    let _ = std::process::Command::new(opener).arg(url).spawn();
}

struct Request {
    method: String,
    path: String,
    body: String,
}

fn read_request(stream: &mut TcpStream) -> Result<Option<Request>> {
    let mut reader = BufReader::new(stream);
    let mut request_line = String::new();
    if reader.read_line(&mut request_line)? == 0 {
        return Ok(None); // connection closed
    }
    let mut parts = request_line.split_whitespace();
    let method = parts.next().unwrap_or_default().to_string();
    let path = parts.next().unwrap_or_default().to_string();

    let mut content_length = 0usize;
    loop {
        let mut line = String::new();
        if reader.read_line(&mut line)? == 0 {
            break;
        }
        let line = line.trim_end();
        if line.is_empty() {
            break; // end of headers
        }
        if let Some(value) = line
            .split_once(':')
            .filter(|(k, _)| k.eq_ignore_ascii_case("content-length"))
            .map(|(_, v)| v.trim())
        {
            content_length = value.parse().unwrap_or(0);
        }
    }

    let mut body = vec![0u8; content_length];
    if content_length > 0 {
        reader.read_exact(&mut body)?;
    }
    Ok(Some(Request {
        method,
        path,
        body: String::from_utf8_lossy(&body).into_owned(),
    }))
}

fn handle_connection(
    stream: &mut TcpStream,
    core: &mut CoreExecution,
    conn: &ResourceAny,
) -> Result<()> {
    let request = match read_request(stream)? {
        Some(r) => r,
        None => return Ok(()),
    };

    match (request.method.as_str(), request.path.as_str()) {
        ("GET", "/") => write_response(stream, 200, "text/html; charset=utf-8", CONSOLE_HTML),
        ("POST", "/api/query") => {
            let json = run_query(core, conn, request.body.trim());
            write_response(stream, 200, "application/json", &json)
        }
        ("GET", "/favicon.ico") => write_response(stream, 204, "text/plain", ""),
        _ => write_response(stream, 404, "text/plain", "not found"),
    }
}

/// Execute SQL against the core and return a JSON body
/// `{"columns":[...],"rows":[[...]],"rowcount":N}` or `{"error":"..."}`.
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

fn json_value(out: &mut String, val: &core_types::Duckvalue) {
    match val {
        core_types::Duckvalue::Null => out.push_str("null"),
        core_types::Duckvalue::Boolean(b) => out.push_str(if *b { "true" } else { "false" }),
        core_types::Duckvalue::Int64(v) => out.push_str(&v.to_string()),
        core_types::Duckvalue::Uint64(v) => out.push_str(&v.to_string()),
        core_types::Duckvalue::Float64(v) => {
            if v.is_finite() {
                out.push_str(&v.to_string());
            } else {
                json_string(out, &v.to_string()); // NaN/Inf aren't valid JSON numbers
            }
        }
        core_types::Duckvalue::Text(s) => json_string(out, s),
        core_types::Duckvalue::Blob(b) => json_string(out, &format!("\\x{} bytes", b.len())),
    }
}

fn duckerror_message(err: &core_types::Duckerror) -> String {
    match err {
        core_types::Duckerror::Invalidargument(m)
        | core_types::Duckerror::Unsupported(m)
        | core_types::Duckerror::Invalidstate(m)
        | core_types::Duckerror::Io(m)
        | core_types::Duckerror::Internal(m) => m.clone(),
    }
}

fn json_error(msg: &str) -> String {
    let mut out = String::from("{\"error\":");
    json_string(&mut out, msg);
    out.push('}');
    out
}

fn json_string(out: &mut String, s: &str) {
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

fn write_response(
    stream: &mut TcpStream,
    status: u16,
    content_type: &str,
    body: &str,
) -> Result<()> {
    let reason = match status {
        200 => "OK",
        204 => "No Content",
        404 => "Not Found",
        _ => "OK",
    };
    let response = format!(
        "HTTP/1.1 {status} {reason}\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nConnection: close\r\nAccess-Control-Allow-Origin: *\r\n\r\n{body}",
        body.len()
    );
    stream.write_all(response.as_bytes())?;
    stream.flush()?;
    Ok(())
}

const CONSOLE_HTML: &str = include_str!("ui_console.html");
