//! Minimal HTTP/1.1 GET client over wasi:sockets, with TLS for https:// via a
//! pure-Rust stack (rustls + rustls-rustcrypto + webpki-roots). This is the same
//! transport the `httpclient` component uses; here it serves Unity Catalog REST
//! GETs with a Bearer token. Returns the decoded response body on 2xx, or an
//! error string. Network access requires the host's network grant.
use std::io::{Read, Write};
use std::net::TcpStream;
use std::sync::Arc;

/// Parsed pieces of an `http(s)://host[:port]/path?query` URL.
/// Returns (host, port, path_with_query, is_tls).
pub fn parse_url(url: &str) -> Option<(String, u16, String, bool)> {
    let url = url.trim();
    let (tls, rest, default_port) = if let Some(r) = url.strip_prefix("https://") {
        (true, r, 443u16)
    } else if let Some(r) = url.strip_prefix("http://") {
        (false, r, 80u16)
    } else {
        return None;
    };
    let (authority, path) = match rest.find('/') {
        Some(i) => (&rest[..i], &rest[i..]),
        None => (rest, "/"),
    };
    let (host, port) = match authority.rsplit_once(':') {
        Some((h, p)) => (h.to_string(), p.parse().ok()?),
        None => (authority.to_string(), default_port),
    };
    if host.is_empty() {
        return None;
    }
    Some((
        host,
        port,
        if path.is_empty() { "/".into() } else { path.to_string() },
        tls,
    ))
}

fn request(host: &str, path: &str, bearer: Option<&str>) -> String {
    let auth = match bearer {
        Some(t) if !t.is_empty() => format!("Authorization: Bearer {t}\r\n"),
        _ => String::new(),
    };
    format!(
        "GET {path} HTTP/1.1\r\nHost: {host}\r\nUser-Agent: ducklink-unityscan/0.1\r\nAccept: application/json\r\n{auth}Connection: close\r\n\r\n"
    )
}

/// Split a raw HTTP/1.1 response into (status, headers, body). Handles
/// `Transfer-Encoding: chunked` (UC servers / proxies commonly chunk JSON).
fn parse_response(raw: &[u8]) -> Option<(u16, String)> {
    // Find header/body boundary on raw bytes (body may be binary/chunked).
    let sep = find_subslice(raw, b"\r\n\r\n")?;
    let head = String::from_utf8_lossy(&raw[..sep]);
    let body_bytes = &raw[sep + 4..];

    let status: u16 = head
        .lines()
        .next()?
        .split_whitespace()
        .nth(1)?
        .parse()
        .ok()?;

    let chunked = head
        .lines()
        .any(|l| {
            let l = l.to_ascii_lowercase();
            l.starts_with("transfer-encoding:") && l.contains("chunked")
        });

    let body = if chunked {
        dechunk(body_bytes)
    } else {
        body_bytes.to_vec()
    };
    Some((status, String::from_utf8_lossy(&body).into_owned()))
}

/// Decode an HTTP/1.1 chunked body into the raw payload. On any malformed input
/// it returns whatever was decoded so far (never panics).
fn dechunk(mut data: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();
    loop {
        let line_end = match find_subslice(data, b"\r\n") {
            Some(i) => i,
            None => break,
        };
        let size_line = String::from_utf8_lossy(&data[..line_end]);
        // chunk-size is hex, optionally followed by ';' extensions.
        let hex = size_line.split(';').next().unwrap_or("").trim();
        let size = match usize::from_str_radix(hex, 16) {
            Ok(s) => s,
            Err(_) => break,
        };
        let chunk_start = line_end + 2;
        if size == 0 {
            break; // last chunk
        }
        if chunk_start + size > data.len() {
            // truncated; salvage what we have
            out.extend_from_slice(&data[chunk_start..]);
            break;
        }
        out.extend_from_slice(&data[chunk_start..chunk_start + size]);
        // advance past chunk data + trailing CRLF
        data = &data[(chunk_start + size + 2).min(data.len())..];
    }
    out
}

fn find_subslice(hay: &[u8], needle: &[u8]) -> Option<usize> {
    if needle.is_empty() || hay.len() < needle.len() {
        return None;
    }
    hay.windows(needle.len()).position(|w| w == needle)
}

fn fetch_plain(host: &str, port: u16, path: &str, bearer: Option<&str>) -> Result<(u16, String), String> {
    let mut stream = TcpStream::connect((host, port)).map_err(|e| format!("connect: {e}"))?;
    stream
        .write_all(request(host, path, bearer).as_bytes())
        .map_err(|e| format!("write: {e}"))?;
    let mut raw = Vec::new();
    stream.read_to_end(&mut raw).map_err(|e| format!("read: {e}"))?;
    parse_response(&raw).ok_or_else(|| "malformed HTTP response".to_string())
}

fn fetch_tls(host: &str, port: u16, path: &str, bearer: Option<&str>) -> Result<(u16, String), String> {
    let root_store = rustls::RootCertStore {
        roots: webpki_roots::TLS_SERVER_ROOTS.to_vec(),
    };
    let config = rustls::ClientConfig::builder_with_provider(Arc::new(rustls_rustcrypto::provider()))
        .with_safe_default_protocol_versions()
        .map_err(|e| format!("tls config: {e}"))?
        .with_root_certificates(root_store)
        .with_no_client_auth();
    let server_name = rustls::pki_types::ServerName::try_from(host.to_string())
        .map_err(|e| format!("server name: {e}"))?;
    let mut conn = rustls::ClientConnection::new(Arc::new(config), server_name)
        .map_err(|e| format!("tls conn: {e}"))?;
    let mut sock = TcpStream::connect((host, port)).map_err(|e| format!("connect: {e}"))?;
    let mut tls = rustls::Stream::new(&mut conn, &mut sock);
    tls.write_all(request(host, path, bearer).as_bytes())
        .map_err(|e| format!("tls write: {e}"))?;
    // Servers often close without a TLS close_notify; tolerate that and keep
    // whatever bytes we received.
    let mut raw = Vec::new();
    let _ = tls.read_to_end(&mut raw);
    if raw.is_empty() {
        return Err("empty TLS response".to_string());
    }
    parse_response(&raw).ok_or_else(|| "malformed HTTP response".to_string())
}

/// GET `url` with an optional Bearer `token`. Returns the response body on a 2xx
/// status, otherwise an error string carrying the status + a body snippet.
pub fn get(url: &str, token: Option<&str>) -> Result<String, String> {
    let (host, port, path, tls) = parse_url(url).ok_or_else(|| format!("bad url '{url}'"))?;
    let (status, body) = if tls {
        fetch_tls(&host, port, &path, token)?
    } else {
        fetch_plain(&host, port, &path, token)?
    };
    if (200..300).contains(&status) {
        Ok(body)
    } else {
        let snippet: String = body.chars().take(200).collect();
        Err(format!("HTTP {status} from {url}: {snippet}"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_url_https_with_query() {
        let (h, p, path, tls) =
            parse_url("https://uc.example.com/api/2.1/unity-catalog/schemas?catalog_name=main")
                .unwrap();
        assert_eq!(h, "uc.example.com");
        assert_eq!(p, 443);
        assert_eq!(path, "/api/2.1/unity-catalog/schemas?catalog_name=main");
        assert!(tls);
    }

    #[test]
    fn parse_url_http_with_port() {
        let (h, p, path, tls) = parse_url("http://localhost:8080/api/2.1/unity-catalog/catalogs").unwrap();
        assert_eq!(h, "localhost");
        assert_eq!(p, 8080);
        assert_eq!(path, "/api/2.1/unity-catalog/catalogs");
        assert!(!tls);
    }

    #[test]
    fn dechunk_decodes() {
        // "Wiki" + "pedia" as two chunks, then terminator.
        let body = b"4\r\nWiki\r\n5\r\npedia\r\n0\r\n\r\n";
        assert_eq!(dechunk(body), b"Wikipedia");
    }

    #[test]
    fn parse_response_chunked_json() {
        let raw = b"HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nTransfer-Encoding: chunked\r\n\r\nb\r\n{\"schemas\":\r\n2\r\n[]\r\n1\r\n}\r\n0\r\n\r\n";
        let (status, body) = parse_response(raw).unwrap();
        assert_eq!(status, 200);
        assert_eq!(body, "{\"schemas\":[]}");
    }
}
