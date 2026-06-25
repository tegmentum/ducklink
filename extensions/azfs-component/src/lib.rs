//! azfs: an az:// (Azure Blob) FileSystem on DuckDB's EXISTING files capability
//! (the webfs pattern). Registers as the files backend and serves byte ranges
//! out of a once-fetched, cached blob body so `read_csv('az://...')` (and any
//! other file reader) works on the lean wasm core with NO built-in azure
//! extension and NO core/host change.
//!
//! az:// -> HTTPS mapping:
//!   az://<container>/<blob>  ->  https://<account>.blob.core.windows.net/<container>/<blob>
//! where <account> comes from AZURE_STORAGE_ACCOUNT / a connection string.
//!
//! Auth (resolved from env vars / connection string):
//!   * PRIMARY: SAS token  -> append `?<sas>` to the blob URL (no signing).
//!   * Shared Key          -> HMAC-SHA256 `Authorization` header over the
//!                            canonicalized request (see `azure::sign_shared_key`).
//!
//! Transport: a single HTTP/1.1 GET of the whole blob over the wasi:sockets
//! graft with pure-Rust TLS (rustls + rustls-rustcrypto + webpki-roots), same
//! stack as httpclient-component. The body is cached; reads slice it.
//!
//! Network-gated like webfs/httpclient (the host grants network per-extension).
use std::cell::RefCell;
use std::collections::HashMap;
use std::io::{Read, Write};
use std::net::TcpStream;
use std::sync::Arc;

use wit_bindgen::rt::string::String as WitString;
use wit_bindgen::rt::vec::Vec as WitVec;

mod azure;
use azure::{
    blob_host_and_path, build_sas_url, parse_az_url, resolve_credentials, sign_shared_key,
};

wit_bindgen::generate!({ path: "./wit", world: "duckdb:extension/duckdb-extension-files" });

use duckdb::extension::{files_reg, runtime, types};
use exports::duckdb::extension::{callback_dispatch, file_dispatch, guest};

/// Opaque callback handle the host passes back to every file-dispatch call.
const FILES_HANDLE: u32 = 1;
/// Opaque handle for the single registered `azfs_version` marker scalar.
const VERSION_HANDLE: u32 = 1;
/// x-ms-version we negotiate for Shared Key requests.
const X_MS_VERSION: &str = "2021-08-06";

struct Extension;

thread_local! {
    /// Cached blob bodies, keyed by the file handle we hand back at open.
    static FILES: RefCell<HashMap<u32, std::vec::Vec<u8>>> = RefCell::new(HashMap::new());
    /// Next file handle to allocate.
    static NEXT_FILE: RefCell<u32> = RefCell::new(1);
}

impl guest::Guest for Extension {
    fn load() -> Result<types::Loadresult, types::Duckerror> {
        // Declare this component as the files backend so the host routes
        // az:// reads here.
        files_reg::register_files(FILES_HANDLE)?;
        register_marker()?;
        Ok(types::Loadresult {
            name: "azfs".into(),
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

// --- Azure Blob GET over wasi:sockets + TLS ---

/// Resolve credentials, map the az:// URL to an HTTPS blob request (SAS-append
/// preferred, else Shared Key signing), GET it over TLS, and return the body.
fn azure_get(url: &str) -> Result<std::vec::Vec<u8>, std::string::String> {
    let p = parse_az_url(url)?;
    let creds = resolve_credentials(|k| std::env::var(k).ok());
    let (host, path) = blob_host_and_path(&creds, &p)?;

    // Path-style override (Azurite / any explicit blob endpoint): the request URL
    // is {endpoint}/{container}/{blob} (the endpoint carries the account path).
    // Virtual-hosted default stays https://{host}{path}.
    let endpoint_url = creds
        .blob_endpoint
        .as_deref()
        .map(|ep| format!("{}/{}/{}", ep.trim().trim_end_matches('/'), p.container, p.blob));

    // PRIMARY path: SAS token -> just append the query, no signing.
    if let Some(sas) = creds.sas_token.as_deref() {
        let base = endpoint_url.unwrap_or_else(|| build_sas_url(&host, &path, ""));
        let base = base.trim_end_matches('?');
        let sas = sas.trim().trim_start_matches('?');
        let full = format!("{base}?{sas}");
        return azure_get_url(&host, &full, &[]);
    }

    // Fallback: Shared Key signing (HMAC-SHA256 Authorization header).
    if let Some(key) = creds.account_key.as_deref() {
        let account = creds
            .account
            .as_deref()
            .ok_or_else(|| "azfs: Shared Key requires an account name".to_string())?;
        let x_ms_date = rfc1123_now();
        let signed = sign_shared_key(account, key, &host, &path, &x_ms_date, X_MS_VERSION)?;
        // SharedKey signs `path` (canon /<account><path>); the request goes to the
        // path-style endpoint URL when set, else the signed virtual-hosted URL.
        let req_url = endpoint_url.unwrap_or(signed.url);
        return azure_get_url(&host, &req_url, &signed.headers);
    }

    Err(format!(
        "azfs: no credentials for '{url}' (set AZURE_STORAGE_SAS_TOKEN, or \
         AZURE_STORAGE_KEY for Shared Key, or AZURE_STORAGE_CONNECTION_STRING)"
    ))
}

/// HTTP/1.1 GET over TLS of a full https:// URL. `host` is the TLS SNI / Host
/// header; `extra_headers` are appended verbatim (used for the Shared Key
/// x-ms-* + Authorization headers).
fn https_get(
    host: &str,
    url: &str,
    extra_headers: &[(std::string::String, std::string::String)],
) -> Result<std::vec::Vec<u8>, std::string::String> {
    // Extract the request-target (path + query) from the URL.
    let after_scheme = url
        .strip_prefix("https://")
        .ok_or_else(|| format!("azfs: expected https url, got '{url}'"))?;
    let target = match after_scheme.find('/') {
        Some(i) => &after_scheme[i..],
        None => "/",
    };

    let mut req = format!(
        "GET {target} HTTP/1.1\r\nHost: {host}\r\nUser-Agent: azfs/0.1\r\nAccept: */*\r\n"
    );
    for (k, v) in extra_headers {
        req.push_str(k);
        req.push_str(": ");
        req.push_str(v);
        req.push_str("\r\n");
    }
    req.push_str("Connection: close\r\n\r\n");

    let raw = tls_roundtrip(host, 443, req.as_bytes())?;

    // Split headers / body on the first CRLFCRLF.
    let sep = b"\r\n\r\n";
    let pos = raw
        .windows(sep.len())
        .position(|w| w == sep)
        .ok_or_else(|| "azfs: malformed HTTP response (no header terminator)".to_string())?;
    let head = &raw[..pos];
    let body_start = pos + sep.len();

    let status_line = head.split(|&b| b == b'\n').next().unwrap_or(&[]);
    let status_text = std::string::String::from_utf8_lossy(status_line);
    let code: u16 = status_text
        .split_whitespace()
        .nth(1)
        .and_then(|s| s.parse().ok())
        .ok_or_else(|| format!("azfs: cannot parse status line '{}'", status_text.trim()))?;
    if !(200..300).contains(&code) {
        return Err(format!("azfs: HTTP {code} for '{url}'"));
    }
    Ok(raw[body_start..].to_vec())
}

/// Open a TLS connection to host:port, write `req`, read the full response.
fn tls_roundtrip(
    host: &str,
    port: u16,
    req: &[u8],
) -> Result<std::vec::Vec<u8>, std::string::String> {
    let root_store = rustls::RootCertStore {
        roots: webpki_roots::TLS_SERVER_ROOTS.to_vec(),
    };
    let config = rustls::ClientConfig::builder_with_provider(Arc::new(rustls_rustcrypto::provider()))
        .with_safe_default_protocol_versions()
        .map_err(|e| format!("azfs: tls config: {e}"))?
        .with_root_certificates(root_store)
        .with_no_client_auth();
    let server_name = rustls::pki_types::ServerName::try_from(host.to_string())
        .map_err(|e| format!("azfs: bad server name '{host}': {e}"))?;
    let mut conn = rustls::ClientConnection::new(Arc::new(config), server_name)
        .map_err(|e| format!("azfs: tls connect: {e}"))?;
    let mut sock = TcpStream::connect((host, port))
        .map_err(|e| format!("azfs: connect {host}:{port} failed: {e}"))?;
    let mut tls = rustls::Stream::new(&mut conn, &mut sock);
    tls.write_all(req)
        .map_err(|e| format!("azfs: tls write: {e}"))?;
    let mut raw = std::vec::Vec::new();
    // Servers often close without close_notify; tolerate and keep what we got.
    let _ = tls.read_to_end(&mut raw);
    if raw.is_empty() {
        return Err("azfs: empty TLS response".to_string());
    }
    Ok(raw)
}

/// Plain (non-TLS) HTTP roundtrip, for `http://` endpoints (e.g. Azurite).
fn plain_roundtrip(
    host: &str,
    port: u16,
    req: &[u8],
) -> Result<std::vec::Vec<u8>, std::string::String> {
    let mut sock = TcpStream::connect((host, port))
        .map_err(|e| format!("azfs: connect {host}:{port} failed: {e}"))?;
    sock.write_all(req)
        .map_err(|e| format!("azfs: http write: {e}"))?;
    let mut raw = std::vec::Vec::new();
    sock.read_to_end(&mut raw)
        .map_err(|e| format!("azfs: http read: {e}"))?;
    if raw.is_empty() {
        return Err("azfs: empty HTTP response".to_string());
    }
    Ok(raw)
}

/// Scheme/port-aware GET of a full http(s):// URL. `host_header` is the Host:
/// header value (authority, host[:port]); the connection host/port/scheme come
/// from `url`. Returns the response body (2xx) or an error.
fn azure_get_url(
    host_header: &str,
    url: &str,
    extra_headers: &[(std::string::String, std::string::String)],
) -> Result<std::vec::Vec<u8>, std::string::String> {
    let (tls, rest) = if let Some(r) = url.strip_prefix("https://") {
        (true, r)
    } else if let Some(r) = url.strip_prefix("http://") {
        (false, r)
    } else {
        return Err(format!("azfs: unsupported url scheme: '{url}'"));
    };
    let (authority, target) = match rest.find('/') {
        Some(i) => (&rest[..i], &rest[i..]),
        None => (rest, "/"),
    };
    let default_port: u16 = if tls { 443 } else { 80 };
    let (conn_host, port) = match authority.rsplit_once(':') {
        Some((h, p)) => (h.to_string(), p.parse().unwrap_or(default_port)),
        None => (authority.to_string(), default_port),
    };

    let mut req = format!(
        "GET {target} HTTP/1.1\r\nHost: {host_header}\r\nUser-Agent: azfs/0.1\r\nAccept: */*\r\n"
    );
    for (k, v) in extra_headers {
        req.push_str(k);
        req.push_str(": ");
        req.push_str(v);
        req.push_str("\r\n");
    }
    req.push_str("Connection: close\r\n\r\n");

    let raw = if tls {
        tls_roundtrip(&conn_host, port, req.as_bytes())?
    } else {
        plain_roundtrip(&conn_host, port, req.as_bytes())?
    };

    let sep = b"\r\n\r\n";
    let pos = raw
        .windows(sep.len())
        .position(|w| w == sep)
        .ok_or_else(|| "azfs: malformed HTTP response (no header terminator)".to_string())?;
    let head = &raw[..pos];
    let body_start = pos + sep.len();
    let status_line = head.split(|&b| b == b'\n').next().unwrap_or(&[]);
    let status_text = std::string::String::from_utf8_lossy(status_line);
    let code: u16 = status_text
        .split_whitespace()
        .nth(1)
        .and_then(|s| s.parse().ok())
        .ok_or_else(|| format!("azfs: cannot parse status line '{}'", status_text.trim()))?;
    if !(200..300).contains(&code) {
        let body_preview = std::string::String::from_utf8_lossy(&raw[body_start..])
            .chars()
            .take(300)
            .collect::<std::string::String>();
        return Err(format!("azfs: HTTP {code} for '{url}': {body_preview}"));
    }
    Ok(raw[body_start..].to_vec())
}

/// Best-effort RFC1123 GMT timestamp for x-ms-date. wasip2 exposes a wall clock
/// via std::time::SystemTime; we format it ourselves (no chrono dep).
fn rfc1123_now() -> std::string::String {
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    format_rfc1123(secs)
}

/// Format a unix timestamp (seconds) as `Wdy, DD Mon YYYY HH:MM:SS GMT`.
fn format_rfc1123(unix_secs: u64) -> std::string::String {
    const DAYS: [&str; 7] = ["Thu", "Fri", "Sat", "Sun", "Mon", "Tue", "Wed"]; // 1970-01-01 = Thu
    const MONTHS: [&str; 12] = [
        "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
    ];
    let days_since_epoch = unix_secs / 86_400;
    let secs_of_day = unix_secs % 86_400;
    let (h, mi, s) = (
        secs_of_day / 3600,
        (secs_of_day % 3600) / 60,
        secs_of_day % 60,
    );
    let wday = DAYS[(days_since_epoch % 7) as usize];

    // Civil-from-days (Howard Hinnant's algorithm).
    let z = days_since_epoch as i64 + 719_468;
    let era = z / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = if m <= 2 { y + 1 } else { y };

    format!(
        "{wday}, {d:02} {mon} {year:04} {h:02}:{mi:02}:{s:02} GMT",
        mon = MONTHS[(m - 1) as usize]
    )
}

impl file_dispatch::Guest for Extension {
    fn file_open(handle: u32, url: WitString) -> Result<file_dispatch::FileOpenResult, WitString> {
        if handle != FILES_HANDLE {
            return Err("azfs: unexpected files callback handle".into());
        }
        let body = azure_get(&url)?;
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

    fn file_read(handle: u32, file: u32, offset: u64, len: u32) -> Result<WitVec<u8>, WitString> {
        if handle != FILES_HANDLE {
            return Err("azfs: unexpected files callback handle".into());
        }
        FILES.with(|f| {
            let map = f.borrow();
            let body = map
                .get(&file)
                .ok_or_else(|| format!("azfs: unknown file handle {file}"))?;
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
            return Err("azfs: unexpected files callback handle".into());
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
        Err(types::Duckerror::Unsupported("azfs: no table fns".into()))
    }
    fn call_aggregate(
        _h: u32,
        _r: types::Rowbatch,
    ) -> Result<types::Duckvalue, types::Duckerror> {
        Err(types::Duckerror::Unsupported("azfs: no aggs".into()))
    }
    fn call_pragma(
        _h: u32,
        _a: WitVec<types::Duckvalue>,
    ) -> Result<Option<types::Duckvalue>, types::Duckerror> {
        Err(types::Duckerror::Unsupported("azfs: no pragmas".into()))
    }
    fn call_cast(
        _h: u32,
        _v: types::Duckvalue,
    ) -> Result<types::Duckvalue, types::Duckerror> {
        Err(types::Duckerror::Unsupported("azfs: no casts".into()))
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
        "azfs_version",
        &[],
        &types::Logicaltype::Text,
        runtime::ScalarCallback::new(VERSION_HANDLE),
        Some(&runtime::Funcopts {
            description: Some("azfs (Azure Blob) files backend version".into()),
            tags: vec!["network".into()],
            attributes: types::Funcflags::empty(),
        }),
    )?;
    Ok(())
}

export!(Extension);

#[cfg(test)]
mod date_tests {
    use super::format_rfc1123;

    #[test]
    fn known_epoch() {
        // 0 -> Thu, 01 Jan 1970 00:00:00 GMT
        assert_eq!(format_rfc1123(0), "Thu, 01 Jan 1970 00:00:00 GMT");
    }

    #[test]
    fn known_2009() {
        // 1248697733 = Mon, 27 Jul 2009 12:28:53 GMT
        assert_eq!(
            format_rfc1123(1_248_697_733),
            "Mon, 27 Jul 2009 12:28:53 GMT"
        );
    }
}
