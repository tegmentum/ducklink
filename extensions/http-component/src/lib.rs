//! Minimal HTTP/1.1 client as DuckDB scalars over the wasi:sockets graft, with
//! TLS for https:// via a pure-Rust stack (rustls + rustls-rustcrypto +
//! webpki-roots): http_get(url) -> text (body), http_status(url) -> bigint.
//! Nondeterministic (network). Errors / non-http(s) URL -> NULL.
use std::io::{Read, Write};
use std::net::TcpStream;
use std::sync::Arc;
use wit_bindgen::rt::string::String;
use wit_bindgen::rt::vec::Vec;
wit_bindgen::generate!({ path: "./wit", world: "duckdb:extension/duckdb-extension" });
use duckdb::extension::{runtime, types};
use exports::duckdb::extension::{callback_dispatch, guest};
struct Extension;
impl guest::Guest for Extension {
    fn load() -> Result<types::Loadresult, types::Duckerror> {
        register_scalars()?;
        Ok(types::Loadresult { name: "http".into(), version: Some(env!("CARGO_PKG_VERSION").into()), requires: Vec::new().into() })
    }
    fn reconfigure(_k: Vec<String>) -> Result<bool, types::Duckerror> { Ok(false) }
    fn shutdown() -> Result<bool, types::Duckerror> { Ok(false) }
}
fn url_arg(args: &[types::Duckvalue]) -> Option<String> {
    match args.first() { Some(types::Duckvalue::Text(s)) => Some(s.clone()), _ => None }
}
/// (host, port, path, is_tls)
fn parse(url: &str) -> Option<(std::string::String, u16, std::string::String, bool)> {
    let url = url.trim();
    let (tls, rest, default_port) = if let Some(r) = url.strip_prefix("https://") { (true, r, 443) }
        else if let Some(r) = url.strip_prefix("http://") { (false, r, 80) } else { return None };
    let (authority, path) = match rest.find('/') { Some(i) => (&rest[..i], &rest[i..]), None => (rest, "/") };
    let (host, port) = match authority.rsplit_once(':') {
        Some((h, p)) => (h.to_string(), p.parse().ok()?),
        None => (authority.to_string(), default_port),
    };
    if host.is_empty() { return None; }
    Some((host, port, if path.is_empty() { "/".into() } else { path.to_string() }, tls))
}
fn request(host: &str, path: &str) -> std::string::String {
    format!("GET {} HTTP/1.1\r\nHost: {}\r\nUser-Agent: ducklink-http/0.1\r\nAccept: */*\r\nConnection: close\r\n\r\n", path, host)
}
fn parse_response(raw: &[u8]) -> Option<(u16, std::string::String)> {
    let text = std::string::String::from_utf8_lossy(raw);
    let (head, body) = text.split_once("\r\n\r\n")?;
    let status: u16 = head.lines().next()?.split_whitespace().nth(1)?.parse().ok()?;
    Some((status, body.to_string()))
}
fn fetch_plain(host: &str, port: u16, path: &str) -> Option<(u16, std::string::String)> {
    let mut stream = TcpStream::connect((host, port)).ok()?;
    stream.write_all(request(host, path).as_bytes()).ok()?;
    let mut raw = std::vec::Vec::new();
    stream.read_to_end(&mut raw).ok()?;
    parse_response(&raw)
}
fn fetch_tls(host: &str, port: u16, path: &str) -> Option<(u16, std::string::String)> {
    let root_store = rustls::RootCertStore { roots: webpki_roots::TLS_SERVER_ROOTS.to_vec() };
    let config = rustls::ClientConfig::builder_with_provider(Arc::new(rustls_rustcrypto::provider()))
        .with_safe_default_protocol_versions().ok()?
        .with_root_certificates(root_store)
        .with_no_client_auth();
    let server_name = rustls::pki_types::ServerName::try_from(host.to_string()).ok()?;
    let mut conn = rustls::ClientConnection::new(Arc::new(config), server_name).ok()?;
    let mut sock = TcpStream::connect((host, port)).ok()?;
    let mut tls = rustls::Stream::new(&mut conn, &mut sock);
    tls.write_all(request(host, path).as_bytes()).ok()?;
    // Servers often close without a TLS close_notify; tolerate that and keep
    // whatever bytes we received.
    let mut raw = std::vec::Vec::new();
    let _ = tls.read_to_end(&mut raw);
    if raw.is_empty() { return None; }
    parse_response(&raw)
}
fn fetch(url: &str) -> Option<(u16, std::string::String)> {
    let (host, port, path, tls) = parse(url)?;
    if tls { fetch_tls(&host, port, &path) } else { fetch_plain(&host, port, &path) }
}
impl callback_dispatch::Guest for Extension {
    fn call_scalar_batch(h: u32, rows: Vec<Vec<types::Duckvalue>>, ctx: types::Invokeinfo) -> Result<Vec<types::Duckvalue>, types::Duckerror> {
        let base = ctx.rowindex.unwrap_or(0); let mut out = Vec::with_capacity(rows.len());
        for (i, a) in rows.into_iter().enumerate() {
            out.push(Self::call_scalar(h, a, types::Invokeinfo { rowindex: Some(base + i as u64), iswindow: ctx.iswindow })?);
        }
        Ok(out)
    }
    fn call_scalar(handle: u32, args: Vec<types::Duckvalue>, _c: types::Invokeinfo) -> Result<types::Duckvalue, types::Duckerror> {
        let url = match url_arg(&args) { Some(s) => s, None => return Ok(types::Duckvalue::Null) };
        let res = fetch(&url);
        Ok(match (handle, res) {
            (1, Some((_, body))) => types::Duckvalue::Text(body.into()),
            (2, Some((status, _))) => types::Duckvalue::Int64(status as i64),
            _ => types::Duckvalue::Null,
        })
    }
    fn call_table(_h: u32, _a: Vec<types::Duckvalue>) -> Result<types::Resultset, types::Duckerror> { Err(types::Duckerror::Unsupported("http: no table fns".into())) }
    fn call_aggregate(_h: u32, _r: types::Rowbatch) -> Result<types::Duckvalue, types::Duckerror> { Err(types::Duckerror::Unsupported("http: no aggs".into())) }
    fn call_pragma(_h: u32, _a: Vec<types::Duckvalue>) -> Result<Option<types::Duckvalue>, types::Duckerror> { Err(types::Duckerror::Unsupported("http: no pragmas".into())) }
    fn call_cast(_h: u32, _v: types::Duckvalue) -> Result<types::Duckvalue, types::Duckerror> { Err(types::Duckerror::Unsupported("http: no casts".into())) }
}
export!(Extension);
fn register_scalars() -> Result<(), types::Duckerror> {
    let cap = runtime::get_capability(types::Capabilitykind::Scalar).ok_or_else(|| types::Duckerror::Internal("no scalar capability".into()))?;
    let reg = match cap { runtime::Capability::Scalar(r) => r, _ => return Err(types::Duckerror::Internal("bad capability".into())) };
    let net = types::Funcflags::empty();
    reg.register("http_get", &[runtime::Funcarg { name: Some("url".into()), logical: types::Logicaltype::Text }],
        types::Logicaltype::Text, runtime::ScalarCallback::new(1),
        Some(&runtime::Funcopts { description: Some("HTTP(S) GET body".into()), tags: vec!["network".into()], attributes: net }))?;
    reg.register("http_status", &[runtime::Funcarg { name: Some("url".into()), logical: types::Logicaltype::Text }],
        types::Logicaltype::Int64, runtime::ScalarCallback::new(2),
        Some(&runtime::Funcopts { description: Some("HTTP(S) status code".into()), tags: vec!["network".into()], attributes: net }))?;
    Ok(())
}
