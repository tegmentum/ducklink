//! Host-bridged quack RPC server for the wasm DuckDB core.
//!
//! quack's httplib server can't `listen()` inside the wasip2 sandbox (the same
//! select/poll gap that broke the httplib client + the ui server). Mirroring
//! `ui_server.rs`, the NATIVE host owns the listening socket + a single-threaded
//! accept loop and bridges each `POST /quack` body into the core, where quack's
//! own request handler runs: `QuackServer::HandleMessage` (DuckDB-internal
//! (de)serialization, lossless) reached via the `handle-quack-request` WIT export
//! -> the `duckdb_quack_handle_request` C bridge in the statically-linked quack
//! extension (cmake/quack-deps/quack_wasi_bridge.cpp).
//!
//! The wire protocol is quack-over-HTTP/1.1 (the client speaks it via DuckDB's
//! HTTPUtil): `POST /quack` with the serialized request as the body and the
//! serialized response returned as `application/vnd.duckdb`; `GET /` is a banner;
//! `OPTIONS /quack` is the CORS preflight. Session/connection state lives in the
//! core's bridge-server singleton (keyed by connection-id inside the messages),
//! so each TCP connection is independent and we answer one request then close.
//!
//! Single-threaded by design: localhost, one core instance, one connection in
//! the accept loop at a time.

use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::Path;
use std::sync::{Arc, Mutex};

use anyhow::{Context, Result};

use super::{
    build_engine, build_wasi_ctx_inherit, instantiate_core, ComponentArtifacts, CoreExecution,
    ExtensionManager,
};

const QUACK_CONTENT_TYPE: &str = "application/vnd.duckdb";

/// Serve the quack RPC protocol on 127.0.0.1:`port`. Blocks forever.
///
/// `token` authenticates clients (the client passes it on connect; quack's
/// handler validates it). Pass the same value to the client, e.g.
/// `quack_query('quack:localhost:<port>', '...', token := '<token>')`.
pub fn serve_quack(
    artifacts: &ComponentArtifacts,
    db: Option<&str>,
    port: u16,
    token: &str,
    preopen_refs: &[(&Path, &str)],
) -> Result<()> {
    let engine = build_engine()?;
    let wasi = build_wasi_ctx_inherit(&[String::from("quack")], preopen_refs)?;
    let manager = Arc::new(Mutex::new(ExtensionManager::new(engine.clone())));
    let mut core = instantiate_core(&engine, &artifacts.core_component, wasi, manager)
        .context("failed to instantiate the core component")?;

    let db_arg = match db {
        None | Some(":memory:") | Some("") => None,
        Some(path) => Some(path.to_string()),
    };
    // Static-linked everything: disable autoinstall/autoload + set a writable
    // extension-data home so open succeeds in the sandbox (as the ui server does).
    let open_opts: Vec<(String, String)> = vec![
        ("autoinstall_known_extensions".to_string(), "false".to_string()),
        ("autoload_known_extensions".to_string(), "false".to_string()),
    ];
    let conn = core
        .with_database(|g, s| g.call_open_with_config(s, db_arg.as_deref(), &open_opts))?
        .map_err(|e| anyhow::anyhow!("open database: {e}"))?;

    // Build the core-side bridge server (no socket bind on wasi -- CreateServer
    // routes to the listen-less WasiQuackServer). This registers the bridge
    // singleton the handle-quack-request export dispatches to.
    let serve_sql = format!(
        "SELECT * FROM quack_serve('quack:localhost:{port}', token := '{}', allow_other_hostname := true)",
        token.replace('\'', "''")
    );
    core.with_database(|g, s| g.call_execute(s, conn.clone(), &serve_sql))?
        .map_err(|e| {
            anyhow::anyhow!("quack_serve bridge init failed: {}", crate::ui_server::duckerror_message(&e))
        })?;

    let listener = TcpListener::bind(("127.0.0.1", port))
        .with_context(|| format!("could not bind 127.0.0.1:{port}"))?;
    eprintln!(
        "quack: serving the quack RPC protocol at http://127.0.0.1:{port}/ (token {token})\n\
         quack: connect a DuckDB quack client, e.g.\n\
         quack:   SELECT * FROM quack_query('quack:localhost:{port}', 'SELECT 42', token := '{token}');"
    );

    for stream in listener.incoming() {
        let mut stream = match stream {
            Ok(s) => s,
            Err(e) => {
                eprintln!("quack: accept error: {e}");
                continue;
            }
        };
        if let Err(e) = handle_connection(&mut stream, &mut core, &conn) {
            eprintln!("quack: request error: {e}");
        }
    }
    Ok(())
}

struct Request {
    method: String,
    path: String,
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
    }

    let mut body = vec![0u8; content_length];
    if content_length > 0 {
        reader.read_exact(&mut body)?;
    }
    Ok(Some(Request { method, path, body }))
}

fn handle_connection(
    stream: &mut TcpStream,
    core: &mut CoreExecution,
    _conn: &wasmtime::component::ResourceAny,
) -> Result<()> {
    let req = match read_request(stream)? {
        Some(r) => r,
        None => return Ok(()),
    };

    match (req.method.as_str(), req.path.as_str()) {
        ("GET", "/") => write_response(
            stream,
            200,
            "text/plain",
            b"This is a DuckDB Quack RPC endpoint. Use ATTACH 'quack:...' to connect here.\n",
        ),
        ("OPTIONS", "/quack") => write_response(stream, 204, "text/plain", b""),
        ("POST", "/quack") => match bridge_quack_request(core, req.body) {
            Some(body) => write_response(stream, 200, QUACK_CONTENT_TYPE, &body),
            None => write_response(stream, 503, "text/plain", b"quack bridge server not started"),
        },
        _ => write_response(stream, 404, "text/plain", b"not found"),
    }
}

/// Forward a serialized quack request body to the component's bridged handler;
/// returns the serialized response body (`application/vnd.duckdb`).
fn bridge_quack_request(core: &mut CoreExecution, body: Vec<u8>) -> Option<Vec<u8>> {
    core.with_database(|g, s| g.call_handle_quack_request(s, &body))
        .ok()?
}

fn write_response(stream: &mut TcpStream, status: u16, content_type: &str, body: &[u8]) -> Result<()> {
    let reason = match status {
        200 => "OK",
        204 => "No Content",
        404 => "Not Found",
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
