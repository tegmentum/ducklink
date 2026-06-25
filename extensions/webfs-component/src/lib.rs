//! httpfs M2 files backend (webfs). Fetches http:// resources over the
//! wasi:sockets graft and serves them to DuckDB's core WasmFileSystem so
//! `read_csv('http://...')` (and any other file reader) works on the lean wasm
//! core with no built-in httpfs.
//!
//! Protocol (host -> component via `file-dispatch`):
//!   * file_open(url): single HTTP/1.1 GET of the WHOLE resource, cache the body
//!     bytes, return a new file handle + the body size. (No Range; that's M3.)
//!   * file_read(file, offset, len): slice the cached body, clamped at EOF.
//!   * file_close(file): drop the cache entry.
//!
//! Network-gated like httpclient (the host grants network per-extension via
//! DUCKLINK_NETWORK_GRANT). M2 supports http:// only; https:// returns an error.
use std::cell::RefCell;
use std::collections::HashMap;
use std::io::{Read, Write};
use std::net::TcpStream;

use wit_bindgen::rt::string::String as WitString;
use wit_bindgen::rt::vec::Vec as WitVec;

wit_bindgen::generate!({ path: "./wit", world: "duckdb:extension/duckdb-extension-files" });

use duckdb::extension::{files_reg, runtime, types};
use exports::duckdb::extension::{callback_dispatch, file_dispatch, guest};

/// Opaque callback handle the host passes back to every file-dispatch call.
const FILES_HANDLE: u32 = 1;
/// Opaque handle for the single registered `webfs_version` marker scalar.
const VERSION_HANDLE: u32 = 1;

struct Extension;

thread_local! {
    /// Cached resource bodies, keyed by the file handle we hand back at open.
    static FILES: RefCell<HashMap<u32, std::vec::Vec<u8>>> = RefCell::new(HashMap::new());
    /// Next file handle to allocate.
    static NEXT_FILE: RefCell<u32> = RefCell::new(1);
}

impl guest::Guest for Extension {
    fn load() -> Result<types::Loadresult, types::Duckerror> {
        // Declare this component as the files backend so the host routes
        // http(s):// reads here.
        files_reg::register_files(FILES_HANDLE)?;
        // A harmless marker scalar so the extension also surfaces a callable
        // function (and is a well-formed loadable extension).
        register_marker()?;
        Ok(types::Loadresult {
            name: "webfs".into(),
            version: Some(env!("CARGO_PKG_VERSION").into()),
            requires: WitVec::new().into(),
        })
    }
    fn reconfigure(_k: WitVec<WitString>) -> Result<bool, types::Duckerror> {
        Ok(false)
    }
    fn shutdown() -> Result<bool, types::Duckerror> {
        Ok(false)
    }
}

// --- HTTP/1.1 GET over wasi:sockets (http:// only for M2) ---

/// (host, port, path)
fn parse_http(url: &str) -> Result<(std::string::String, u16, std::string::String), std::string::String> {
    let url = url.trim();
    if url.starts_with("https://") {
        return Err("webfs M2: https:// not supported (http:// only)".to_string());
    }
    let rest = url
        .strip_prefix("http://")
        .ok_or_else(|| format!("webfs: not an http:// url: '{url}'"))?;
    let (authority, path) = match rest.find('/') {
        Some(i) => (&rest[..i], &rest[i..]),
        None => (rest, "/"),
    };
    let (host, port) = match authority.rsplit_once(':') {
        Some((h, p)) => (
            h.to_string(),
            p.parse::<u16>().map_err(|_| format!("webfs: bad port in '{url}'"))?,
        ),
        None => (authority.to_string(), 80u16),
    };
    if host.is_empty() {
        return Err(format!("webfs: empty host in '{url}'"));
    }
    Ok((host, port, if path.is_empty() { "/".into() } else { path.to_string() }))
}

/// Fetch the whole resource and return its body bytes.
fn http_get(url: &str) -> Result<std::vec::Vec<u8>, std::string::String> {
    let (host, port, path) = parse_http(url)?;
    // Literal SocketAddr connect (host:port) — wasi:sockets handles the
    // loopback/numeric IP case; named hosts go through allow_ip_name_lookup.
    let mut stream = TcpStream::connect((host.as_str(), port))
        .map_err(|e| format!("webfs: connect {host}:{port} failed: {e}"))?;
    let req = format!(
        "GET {} HTTP/1.1\r\nHost: {}\r\nUser-Agent: webfs/0.1\r\nAccept: */*\r\nConnection: close\r\n\r\n",
        path, host
    );
    stream
        .write_all(req.as_bytes())
        .map_err(|e| format!("webfs: send failed: {e}"))?;
    let mut raw = std::vec::Vec::new();
    stream
        .read_to_end(&mut raw)
        .map_err(|e| format!("webfs: read failed: {e}"))?;
    // Split headers / body on the first CRLFCRLF. The body is returned as-is
    // (M2 assumes identity transfer-encoding; the test server sends Content-Length).
    let sep = b"\r\n\r\n";
    let pos = raw
        .windows(sep.len())
        .position(|w| w == sep)
        .ok_or_else(|| "webfs: malformed HTTP response (no header terminator)".to_string())?;
    let head = &raw[..pos];
    let body_start = pos + sep.len();
    // Validate the status line is 2xx.
    let status_line = head.split(|&b| b == b'\n').next().unwrap_or(&[]);
    let status_text = std::string::String::from_utf8_lossy(status_line);
    let code: u16 = status_text
        .split_whitespace()
        .nth(1)
        .and_then(|s| s.parse().ok())
        .ok_or_else(|| format!("webfs: cannot parse status line '{}'", status_text.trim()))?;
    if !(200..300).contains(&code) {
        return Err(format!("webfs: HTTP {code} for '{url}'"));
    }
    Ok(raw[body_start..].to_vec())
}

impl file_dispatch::Guest for Extension {
    fn file_open(
        handle: u32,
        url: WitString,
    ) -> Result<file_dispatch::FileOpenResult, WitString> {
        if handle != FILES_HANDLE {
            return Err("webfs: unexpected files callback handle".into());
        }
        let body = http_get(&url)?;
        let size = body.len() as u64;
        let id = NEXT_FILE.with(|n| {
            let mut n = n.borrow_mut();
            let id = *n;
            *n = n.wrapping_add(1).max(1);
            id
        });
        FILES.with(|f| f.borrow_mut().insert(id, body));
        Ok(file_dispatch::FileOpenResult { handle: id, size })
    }

    fn file_read(
        handle: u32,
        file: u32,
        offset: u64,
        len: u32,
    ) -> Result<WitVec<u8>, WitString> {
        if handle != FILES_HANDLE {
            return Err("webfs: unexpected files callback handle".into());
        }
        FILES.with(|f| {
            let map = f.borrow();
            let body = map
                .get(&file)
                .ok_or_else(|| format!("webfs: unknown file handle {file}"))?;
            let total = body.len() as u64;
            if offset >= total {
                return Ok(WitVec::new());
            }
            let start = offset as usize;
            let end = std::cmp::min(total, offset + len as u64) as usize;
            Ok(body[start..end].to_vec().into())
        })
    }

    fn file_close(handle: u32, file: u32) -> Result<(), WitString> {
        if handle != FILES_HANDLE {
            return Err("webfs: unexpected files callback handle".into());
        }
        FILES.with(|f| {
            f.borrow_mut().remove(&file);
        });
        Ok(())
    }
}

impl callback_dispatch::Guest for Extension {
    fn call_scalar_batch(
        h: u32,
        rows: WitVec<WitVec<types::Duckvalue>>,
        ctx: types::Invokeinfo,
    ) -> Result<WitVec<types::Duckvalue>, types::Duckerror> {
        let base = ctx.rowindex.unwrap_or(0);
        let mut out = WitVec::with_capacity(rows.len());
        for (i, a) in rows.into_iter().enumerate() {
            out.push(Self::call_scalar(
                h,
                a,
                types::Invokeinfo {
                    rowindex: Some(base + i as u64),
                    iswindow: ctx.iswindow,
                },
            )?);
        }
        Ok(out)
    }
    fn call_scalar(
        _h: u32,
        _args: WitVec<types::Duckvalue>,
        _c: types::Invokeinfo,
    ) -> Result<types::Duckvalue, types::Duckerror> {
        Ok(types::Duckvalue::Text(env!("CARGO_PKG_VERSION").into()))
    }
    fn call_table(
        _h: u32,
        _a: WitVec<types::Duckvalue>,
    ) -> Result<types::Resultset, types::Duckerror> {
        Err(types::Duckerror::Unsupported("webfs: no table fns".into()))
    }
    fn call_aggregate(
        _h: u32,
        _r: types::Rowbatch,
    ) -> Result<types::Duckvalue, types::Duckerror> {
        Err(types::Duckerror::Unsupported("webfs: no aggs".into()))
    }
    fn call_pragma(
        _h: u32,
        _a: WitVec<types::Duckvalue>,
    ) -> Result<Option<types::Duckvalue>, types::Duckerror> {
        Err(types::Duckerror::Unsupported("webfs: no pragmas".into()))
    }
    fn call_cast(
        _h: u32,
        _v: types::Duckvalue,
    ) -> Result<types::Duckvalue, types::Duckerror> {
        Err(types::Duckerror::Unsupported("webfs: no casts".into()))
    }
}

fn register_marker() -> Result<(), types::Duckerror> {
    let cap = runtime::get_capability(types::Capabilitykind::Scalar)
        .ok_or_else(|| types::Duckerror::Internal("no scalar capability".into()))?;
    let reg = match cap {
        runtime::Capability::Scalar(r) => r,
        _ => return Err(types::Duckerror::Internal("bad capability".into())),
    };
    reg.register(
        "webfs_version",
        &[],
        &types::Logicaltype::Text,
        runtime::ScalarCallback::new(VERSION_HANDLE),
        Some(&runtime::Funcopts {
            description: Some("webfs files backend version".into()),
            tags: vec!["network".into()],
            attributes: types::Funcflags::empty(),
        }),
    )?;
    Ok(())
}

export!(Extension);
