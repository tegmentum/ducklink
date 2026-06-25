//! s3fs M2 files backend. Fetches `s3://bucket/key` objects over HTTPS
//! (wasi:sockets graft + pure-Rust rustls TLS, same stack as
//! httpclient-component) and serves them to DuckDB's core WasmFileSystem so
//! `read_csv('s3://...')` / `read_parquet('s3://...')` work on the lean wasm
//! core with no built-in httpfs.
//!
//! This is webfs-component, but for the s3:// scheme:
//!   * An S3 GET of `s3://bucket/key` is an HTTPS GET of the virtual-hosted URL
//!     `https://<bucket>.s3.<region>.amazonaws.com/<url-encoded-key>`.
//!   * If AWS credentials are present (env AWS_ACCESS_KEY_ID / SECRET_ACCESS_KEY,
//!     optional SESSION_TOKEN), the request carries an AWS SigV4 `Authorization`
//!     header. With no credentials we fall back to ANONYMOUS mode (public
//!     buckets) — no auth header.
//!
//! Protocol (host -> component via `file-dispatch`), identical to webfs:
//!   * file_open(url): single HTTPS GET of the WHOLE object, cache the body,
//!     return a new file handle + the body size.
//!   * file_read(file, offset, len): slice the cached body, clamped at EOF.
//!   * file_close(file): drop the cache entry.
//!
//! Network-gated like webfs/httpclient (the host grants network per-extension
//! via DUCKLINK_NETWORK_GRANT=s3fs).
use std::cell::RefCell;
use std::collections::HashMap;
use std::io::{Read, Write};
use std::net::TcpStream;
use std::sync::Arc;

use wit_bindgen::rt::string::String as WitString;
use wit_bindgen::rt::vec::Vec as WitVec;

wit_bindgen::generate!({ path: "./wit", world: "duckdb:extension/duckdb-extension-files" });

use duckdb::extension::{files_reg, runtime, types};
use exports::duckdb::extension::{callback_dispatch, file_dispatch, guest};

mod sigv4;

/// Opaque callback handle the host passes back to every file-dispatch call.
const FILES_HANDLE: u32 = 1;
/// Opaque handle for the single registered `s3fs_version` marker scalar.
const VERSION_HANDLE: u32 = 1;
/// Default region when none is supplied via env (S3's legacy default).
const DEFAULT_REGION: &str = "us-east-1";

struct Extension;

thread_local! {
    /// Cached object bodies, keyed by the file handle we hand back at open.
    static FILES: RefCell<HashMap<u32, std::vec::Vec<u8>>> = RefCell::new(HashMap::new());
    /// Next file handle to allocate.
    static NEXT_FILE: RefCell<u32> = RefCell::new(1);
}

impl guest::Guest for Extension {
    fn load() -> Result<types::Loadresult, types::Duckerror> {
        // Declare this component as the files backend so the host routes
        // s3:// reads here.
        files_reg::register_files(FILES_HANDLE)?;
        register_marker()?;
        Ok(types::Loadresult {
            name: "s3fs".into(),
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

// --- s3:// -> virtual-hosted HTTPS mapping ---

/// Parsed `s3://bucket/key` reference plus the derived request host/path.
struct S3Ref {
    bucket: String,
    key: String,
    region: String,
    /// Connection host (no port) — TLS SNI / TCP connect target.
    host: String,
    /// `Host:` header value (host[:port] when the port is non-default). SigV4
    /// signs this exact value.
    host_header: String,
    port: u16,
    /// http (plain, e.g. minio) vs https (AWS / TLS endpoints).
    tls: bool,
    /// Absolute request path. Virtual-hosted = `/<key>`; path-style (endpoint
    /// override) = `/<bucket>/<key>`. Always starts with '/'.
    path: String,
}

/// Region resolution: AWS_REGION, then AWS_DEFAULT_REGION, then the default.
fn resolved_region() -> String {
    std::env::var("AWS_REGION")
        .or_else(|_| std::env::var("AWS_DEFAULT_REGION"))
        .ok()
        .filter(|s| !s.trim().is_empty())
        .unwrap_or_else(|| DEFAULT_REGION.to_string())
}

/// Parse `s3://bucket/key` into bucket + key. The bucket is everything up to
/// the first '/', the key is the remainder (may itself contain '/').
fn parse_s3(url: &str) -> Result<(String, String), String> {
    let rest = url
        .trim()
        .strip_prefix("s3://")
        .ok_or_else(|| format!("s3fs: not an s3:// url: '{url}'"))?;
    let (bucket, key) = match rest.find('/') {
        Some(i) => (&rest[..i], &rest[i + 1..]),
        None => (rest, ""),
    };
    if bucket.is_empty() {
        return Err(format!("s3fs: empty bucket in '{url}'"));
    }
    if key.is_empty() {
        return Err(format!("s3fs: empty key in '{url}'"));
    }
    Ok((bucket.to_string(), key.to_string()))
}

/// Build the request from an `s3://` URL. Default is AWS virtual-hosted HTTPS;
/// `AWS_ENDPOINT_URL` (e.g. http://127.0.0.1:9000 for minio, or an R2/Ceph/Wasabi
/// endpoint) switches to PATH-STYLE against that endpoint (the scheme/port from
/// the URL). SigV4 signs whatever host+path we actually send, so a custom
/// endpoint stays correctly signed.
fn s3_ref(url: &str) -> Result<S3Ref, String> {
    let (bucket, key) = parse_s3(url)?;
    let region = resolved_region();
    let enc_key = sigv4::uri_encode_path(&key);
    if let Some(ep) = std::env::var("AWS_ENDPOINT_URL")
        .ok()
        .filter(|s| !s.trim().is_empty())
    {
        let (tls, rest) = if let Some(r) = ep.strip_prefix("https://") {
            (true, r)
        } else if let Some(r) = ep.strip_prefix("http://") {
            (false, r)
        } else {
            (true, ep.as_str())
        };
        let rest = rest.trim_end_matches('/');
        let default_port: u16 = if tls { 443 } else { 80 };
        let (h, port) = match rest.rsplit_once(':') {
            Some((h, p)) => (h.to_string(), p.parse().unwrap_or(default_port)),
            None => (rest.to_string(), default_port),
        };
        let host_header = if port == default_port {
            h.clone()
        } else {
            format!("{h}:{port}")
        };
        return Ok(S3Ref {
            path: format!("/{bucket}/{enc_key}"), // path-style
            bucket,
            key,
            region,
            host: h,
            host_header,
            port,
            tls,
        });
    }
    // Default: AWS virtual-hosted-style HTTPS (the regional form is uniform).
    let host = format!("{bucket}.s3.{region}.amazonaws.com");
    Ok(S3Ref {
        path: format!("/{enc_key}"),
        bucket,
        key,
        region,
        host_header: host.clone(),
        host,
        port: 443,
        tls: true,
    })
}

// --- HTTPS GET over wasi:sockets + rustls (mirrors httpclient-component) ---

/// Read AWS credentials from the wasi environment. Returns None for anonymous
/// (public-bucket) access when no access key is set.
fn aws_credentials() -> Option<sigv4::Credentials> {
    let access_key = std::env::var("AWS_ACCESS_KEY_ID").ok()?;
    let secret_key = std::env::var("AWS_SECRET_ACCESS_KEY").ok()?;
    if access_key.trim().is_empty() || secret_key.trim().is_empty() {
        return None;
    }
    let session_token = std::env::var("AWS_SESSION_TOKEN")
        .ok()
        .filter(|s| !s.trim().is_empty());
    Some(sigv4::Credentials {
        access_key,
        secret_key,
        session_token,
    })
}

/// Fetch the whole S3 object over HTTPS and return its body bytes.
fn s3_get(url: &str) -> Result<std::vec::Vec<u8>, String> {
    let s3 = s3_ref(url)?;

    // Assemble the request headers. SigV4 (when credentials are present) signs
    // host + x-amz-date + x-amz-content-sha256 (+ x-amz-security-token).
    let amz_date = sigv4::amz_date_now();
    let payload_hash = sigv4::EMPTY_PAYLOAD_SHA256; // GET has no body
    let mut headers: Vec<(String, String)> = vec![
        ("host".to_string(), s3.host_header.clone()),
        ("x-amz-content-sha256".to_string(), payload_hash.to_string()),
        ("x-amz-date".to_string(), amz_date.clone()),
    ];

    if let Some(creds) = aws_credentials() {
        if let Some(tok) = &creds.session_token {
            headers.push(("x-amz-security-token".to_string(), tok.clone()));
        }
        let authz = sigv4::sign_v4(
            &creds,
            "GET",
            &s3.region,
            "s3",
            &s3.path,
            "", // no query string
            &headers,
            payload_hash,
            &amz_date,
        );
        headers.push(("Authorization".to_string(), authz));
    }
    // else: anonymous mode (public bucket) — no Authorization header.

    let raw = if s3.tls {
        https_get(&s3.host, s3.port, &s3.path, &headers)?
    } else {
        http_get(&s3.host, s3.port, &s3.path, &headers)?
    };

    // Split headers / body on the first CRLFCRLF.
    let sep = b"\r\n\r\n";
    let pos = raw
        .windows(sep.len())
        .position(|w| w == sep)
        .ok_or_else(|| "s3fs: malformed HTTP response (no header terminator)".to_string())?;
    let head = &raw[..pos];
    let body_start = pos + sep.len();
    let status_line = head.split(|&b| b == b'\n').next().unwrap_or(&[]);
    let status_text = std::string::String::from_utf8_lossy(status_line);
    let code: u16 = status_text
        .split_whitespace()
        .nth(1)
        .and_then(|s| s.parse().ok())
        .ok_or_else(|| format!("s3fs: cannot parse status line '{}'", status_text.trim()))?;
    if !(200..300).contains(&code) {
        let body_preview =
            std::string::String::from_utf8_lossy(&raw[body_start..]).chars().take(512).collect::<std::string::String>();
        return Err(format!(
            "s3fs: HTTP {code} for s3://{}/{} ({}): {}",
            s3.bucket, s3.key, s3.host, body_preview
        ));
    }
    Ok(raw[body_start..].to_vec())
}

/// Plain HTTP GET over wasi:sockets (for `AWS_ENDPOINT_URL=http://...`, e.g.
/// minio). Returns the raw (headers+body) bytes.
fn http_get(host: &str, port: u16, path: &str, headers: &[(String, String)]) -> Result<std::vec::Vec<u8>, String> {
    let mut stream = TcpStream::connect((host, port))
        .map_err(|e| format!("s3fs: connect {host}:{port} failed: {e}"))?;
    let mut req = format!("GET {path} HTTP/1.1\r\n");
    for (k, v) in headers {
        req.push_str(k);
        req.push_str(": ");
        req.push_str(v);
        req.push_str("\r\n");
    }
    req.push_str("User-Agent: ducklink-s3fs/0.1\r\nAccept: */*\r\nConnection: close\r\n\r\n");
    stream
        .write_all(req.as_bytes())
        .map_err(|e| format!("s3fs: send failed: {e}"))?;
    let mut raw = std::vec::Vec::new();
    stream
        .read_to_end(&mut raw)
        .map_err(|e| format!("s3fs: read failed: {e}"))?;
    if raw.is_empty() {
        return Err("s3fs: empty response".to_string());
    }
    Ok(raw)
}

/// HTTPS GET over wasi:sockets + rustls; returns the raw (headers+body) bytes.
fn https_get(host: &str, port: u16, path: &str, headers: &[(String, String)]) -> Result<std::vec::Vec<u8>, String> {
    let root_store = rustls::RootCertStore {
        roots: webpki_roots::TLS_SERVER_ROOTS.to_vec(),
    };
    let config = rustls::ClientConfig::builder_with_provider(Arc::new(rustls_rustcrypto::provider()))
        .with_safe_default_protocol_versions()
        .map_err(|e| format!("s3fs: tls config: {e}"))?
        .with_root_certificates(root_store)
        .with_no_client_auth();
    let server_name = rustls::pki_types::ServerName::try_from(host.to_string())
        .map_err(|e| format!("s3fs: bad server name '{host}': {e}"))?;
    let mut conn = rustls::ClientConnection::new(Arc::new(config), server_name)
        .map_err(|e| format!("s3fs: tls connect: {e}"))?;
    let mut sock = TcpStream::connect((host, port))
        .map_err(|e| format!("s3fs: connect {host}:{port} failed: {e}"))?;
    let mut tls = rustls::Stream::new(&mut conn, &mut sock);

    let mut req = format!("GET {path} HTTP/1.1\r\n");
    for (k, v) in headers {
        req.push_str(k);
        req.push_str(": ");
        req.push_str(v);
        req.push_str("\r\n");
    }
    req.push_str("User-Agent: ducklink-s3fs/0.1\r\nAccept: */*\r\nConnection: close\r\n\r\n");

    tls.write_all(req.as_bytes())
        .map_err(|e| format!("s3fs: send failed: {e}"))?;
    // S3 often closes without a TLS close_notify; tolerate that.
    let mut raw = std::vec::Vec::new();
    let _ = tls.read_to_end(&mut raw);
    if raw.is_empty() {
        return Err("s3fs: empty response".to_string());
    }
    Ok(raw)
}

impl file_dispatch::Guest for Extension {
    fn file_open(
        handle: u32,
        url: WitString,
    ) -> Result<file_dispatch::FileOpenResult, WitString> {
        if handle != FILES_HANDLE {
            return Err("s3fs: unexpected files callback handle".into());
        }
        let body = s3_get(&url).map_err(WitString::from)?;
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
            return Err("s3fs: unexpected files callback handle".into());
        }
        FILES.with(|f| {
            let map = f.borrow();
            let body = map
                .get(&file)
                .ok_or_else(|| format!("s3fs: unknown file handle {file}"))?;
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
            return Err("s3fs: unexpected files callback handle".into());
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
        Err(types::Duckerror::Unsupported("s3fs: no table fns".into()))
    }
    fn call_aggregate(
        _h: u32,
        _r: types::Rowbatch,
    ) -> Result<types::Duckvalue, types::Duckerror> {
        Err(types::Duckerror::Unsupported("s3fs: no aggs".into()))
    }
    fn call_pragma(
        _h: u32,
        _a: WitVec<types::Duckvalue>,
    ) -> Result<Option<types::Duckvalue>, types::Duckerror> {
        Err(types::Duckerror::Unsupported("s3fs: no pragmas".into()))
    }
    fn call_cast(
        _h: u32,
        _v: types::Duckvalue,
    ) -> Result<types::Duckvalue, types::Duckerror> {
        Err(types::Duckerror::Unsupported("s3fs: no casts".into()))
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
        "s3fs_version",
        &[],
        &types::Logicaltype::Text,
        runtime::ScalarCallback::new(VERSION_HANDLE),
        Some(&runtime::Funcopts {
            description: Some("s3fs files backend version".into()),
            tags: vec!["network".into()],
            attributes: types::Funcflags::empty(),
        }),
    )?;
    Ok(())
}

export!(Extension);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_s3_url() {
        let (b, k) = parse_s3("s3://my-bucket/path/to/object.parquet").unwrap();
        assert_eq!(b, "my-bucket");
        assert_eq!(k, "path/to/object.parquet");
    }

    #[test]
    fn parses_s3_single_segment_key() {
        let (b, k) = parse_s3("s3://bucket/file.csv").unwrap();
        assert_eq!(b, "bucket");
        assert_eq!(k, "file.csv");
    }

    #[test]
    fn rejects_non_s3() {
        assert!(parse_s3("http://example.com/x").is_err());
        assert!(parse_s3("s3://bucket").is_err()); // empty key
        assert!(parse_s3("s3:///key").is_err()); // empty bucket
    }

    #[test]
    fn builds_virtual_hosted_url() {
        std::env::set_var("AWS_REGION", "eu-west-1");
        let s3 = s3_ref("s3://examplebucket/photos/2015/sample.jpg").unwrap();
        assert_eq!(s3.host, "examplebucket.s3.eu-west-1.amazonaws.com");
        assert_eq!(s3.path, "/photos/2015/sample.jpg");
        std::env::remove_var("AWS_REGION");
    }
}
