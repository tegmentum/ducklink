//! OpenAI-compatible chat-completions client as DuckDB scalars, over the
//! wasi:sockets graft with TLS for https:// via a pure-Rust stack (rustls +
//! rustls-rustcrypto + webpki-roots) -- the exact networking approach reused
//! from the `http` extension, swapped from GET to a POST with a JSON body and
//! Bearer auth, and the response parsed with serde_json.
//!
//!   prompt(text)            -> assistant message text (model from OPENAI_MODEL)
//!   prompt_model(text, m)   -> assistant message text (explicit model)
//!
//! Config is read from the environment:
//!   OPENAI_BASE_URL  (default https://api.openai.com/v1)
//!   OPENAI_MODEL     (default gpt-4o-mini)
//!   OPENAI_API_KEY   (required; missing/empty -> NULL)
//!
//! Nondeterministic (network). Any error -- missing key, network failure,
//! non-2xx status, unparseable JSON -- returns NULL. Never panics.
use std::io::{Read, Write};
use std::net::TcpStream;
use std::sync::Arc;
use wit_bindgen::rt::string::String;
use wit_bindgen::rt::vec::Vec;
wit_bindgen::generate!({ path: "./wit", world: "duckdb:extension/duckdb-extension" });
use duckdb::extension::{runtime, types};
use exports::duckdb::extension::guest;
struct Extension;
impl guest::Guest for Extension {
    fn load() -> Result<types::Loadresult, types::Duckerror> {
        register_scalars()?;
        Ok(types::Loadresult { name: "openprompt".into(), version: Some(env!("CARGO_PKG_VERSION").into()), requires: Vec::new().into() })
    }
    fn reconfigure(_k: Vec<String>) -> Result<bool, types::Duckerror> { Ok(false) }
    fn shutdown() -> Result<bool, types::Duckerror> { Ok(false) }
}

fn text_arg(args: &[types::Duckvalue], i: usize) -> Option<std::string::String> {
    match args.get(i) { Some(types::Duckvalue::Text(s)) => Some(s.to_string()), _ => None }
}

/// (host, port, path) for an https:// base URL. Only https is supported; the
/// host's network grant + rustls handle the TLS. http:// and other schemes -> None.
fn parse_https(url: &str) -> Option<(std::string::String, u16, std::string::String)> {
    let rest = url.trim().strip_prefix("https://")?;
    let (authority, path) = match rest.find('/') { Some(i) => (&rest[..i], &rest[i..]), None => (rest, "") };
    let (host, port) = match authority.rsplit_once(':') {
        Some((h, p)) => (h.to_string(), p.parse().ok()?),
        None => (authority.to_string(), 443u16),
    };
    if host.is_empty() { return None; }
    let path = path.trim_end_matches('/').to_string();
    Some((host, port, path))
}

/// Build the chat-completions request body. We hand-build the JSON so the only
/// serde_json work is escaping the user text safely.
fn request_body(model: &str, text: &str) -> std::string::String {
    let model = serde_json::Value::String(model.to_string());
    let content = serde_json::Value::String(text.to_string());
    let body = serde_json::json!({
        "model": model,
        "messages": [{ "role": "user", "content": content }],
    });
    body.to_string()
}

fn http_request(host: &str, path: &str, api_key: &str, body: &str) -> std::string::String {
    let endpoint = format!("{}/chat/completions", path);
    format!(
        "POST {endpoint} HTTP/1.1\r\nHost: {host}\r\nUser-Agent: ducklink-openprompt/0.1\r\nAuthorization: Bearer {key}\r\nContent-Type: application/json\r\nAccept: */*\r\nContent-Length: {len}\r\nConnection: close\r\n\r\n{body}",
        endpoint = endpoint, host = host, key = api_key, len = body.len(), body = body,
    )
}

fn parse_response(raw: &[u8]) -> Option<(u16, std::string::String)> {
    let text = std::string::String::from_utf8_lossy(raw);
    let (head, body) = text.split_once("\r\n\r\n")?;
    let status: u16 = head.lines().next()?.split_whitespace().nth(1)?.parse().ok()?;
    // De-chunk a Transfer-Encoding: chunked body if present (servers commonly
    // chunk JSON responses on Connection: close).
    let chunked = head.lines().any(|l| {
        let l = l.to_ascii_lowercase();
        l.starts_with("transfer-encoding:") && l.contains("chunked")
    });
    let body = if chunked { dechunk(body).unwrap_or_else(|| body.to_string()) } else { body.to_string() };
    Some((status, body))
}

fn dechunk(body: &str) -> Option<std::string::String> {
    let mut out = std::string::String::new();
    let mut rest = body;
    loop {
        let (size_line, after) = rest.split_once("\r\n")?;
        let size_hex = size_line.split(';').next()?.trim();
        let size = usize::from_str_radix(size_hex, 16).ok()?;
        if size == 0 { break; }
        if after.len() < size { return None; }
        out.push_str(&after[..size]);
        rest = after.get(size + 2..).unwrap_or(""); // skip chunk + trailing CRLF
    }
    Some(out)
}

fn fetch_completion(text: &str, model: &str) -> Option<std::string::String> {
    let api_key = std::env::var("OPENAI_API_KEY").ok()?;
    if api_key.trim().is_empty() { return None; }
    let base = std::env::var("OPENAI_BASE_URL").unwrap_or_else(|_| "https://api.openai.com/v1".to_string());
    let (host, port, path) = parse_https(&base)?;
    let body = request_body(model, text);
    let req = http_request(&host, &path, api_key.trim(), &body);

    // --- networking reused verbatim from the http extension's fetch_tls ---
    let root_store = rustls::RootCertStore { roots: webpki_roots::TLS_SERVER_ROOTS.to_vec() };
    let config = rustls::ClientConfig::builder_with_provider(Arc::new(rustls_rustcrypto::provider()))
        .with_safe_default_protocol_versions().ok()?
        .with_root_certificates(root_store)
        .with_no_client_auth();
    let server_name = rustls::pki_types::ServerName::try_from(host.clone()).ok()?;
    let mut conn = rustls::ClientConnection::new(Arc::new(config), server_name).ok()?;
    let mut sock = TcpStream::connect((host.as_str(), port)).ok()?;
    let mut tls = rustls::Stream::new(&mut conn, &mut sock);
    tls.write_all(req.as_bytes()).ok()?;
    let mut raw = std::vec::Vec::new();
    let _ = tls.read_to_end(&mut raw); // tolerate missing TLS close_notify
    if raw.is_empty() { return None; }
    // --- end reused networking ---

    let (status, resp_body) = parse_response(&raw)?;
    if !(200..300).contains(&status) { return None; }
    extract_message(&resp_body)
}

/// choices[0].message.content from an OpenAI-compatible response.
fn extract_message(body: &str) -> Option<std::string::String> {
    let v: serde_json::Value = serde_json::from_str(body).ok()?;
    let content = v.get("choices")?.get(0)?.get("message")?.get("content")?;
    content.as_str().map(|s| s.to_string())
}

// Per-row scalar logic, UNCHANGED from the major-3 hand-written impl. The
// major-4 columnar dispatch is generated by `columnar_bridge!` below, which
// bridges `colvec` <-> row and delegates here. (openprompt has real network
// scalars, so it migrates via the bridge-scalar path, not bridge-stub.)
fn scalar(handle: u32, args: Vec<types::Duckvalue>, _c: types::Invokeinfo) -> Result<types::Duckvalue, types::Duckerror> {
    let text = match text_arg(&args, 0) { Some(s) => s, None => return Ok(types::Duckvalue::Null) };
    let model = match handle {
        // prompt(text): model from env, default gpt-4o-mini
        1 => std::env::var("OPENAI_MODEL").unwrap_or_else(|_| "gpt-4o-mini".to_string()),
        // prompt_model(text, model): explicit model arg
        2 => match text_arg(&args, 1) { Some(m) => m, None => return Ok(types::Duckvalue::Null) },
        _ => return Ok(types::Duckvalue::Null),
    };
    Ok(match fetch_completion(&text, &model) {
        Some(msg) => types::Duckvalue::Text(msg.into()),
        None => types::Duckvalue::Null,
    })
}
datalink_extcore::columnar_bridge! {
    types = duckdb::extension::types;
    column_types = duckdb::extension::column_types;
    callback_dispatch = exports::duckdb::extension::callback_dispatch;
    target = Extension;
    scalar = scalar;
}
export!(Extension);

fn register_scalars() -> Result<(), types::Duckerror> {
    let cap = runtime::get_capability(types::Capabilitykind::Scalar).ok_or_else(|| types::Duckerror::Internal("no scalar capability".into()))?;
    let reg = match cap { runtime::Capability::Scalar(r) => r, _ => return Err(types::Duckerror::Internal("bad capability".into())) };
    let net = types::Funcflags::empty();
    reg.register("prompt", &[runtime::Funcarg { name: Some("text".into()), logical: types::Logicaltype::Text }],
        &types::Logicaltype::Text, runtime::ScalarCallback::new(1),
        Some(&runtime::Funcopts { description: Some("OpenAI-compatible chat completion (model from OPENAI_MODEL)".into()), tags: vec!["network".into()], attributes: net }))?;
    reg.register("prompt_model",
        &[runtime::Funcarg { name: Some("text".into()), logical: types::Logicaltype::Text },
          runtime::Funcarg { name: Some("model".into()), logical: types::Logicaltype::Text }],
        &types::Logicaltype::Text, runtime::ScalarCallback::new(2),
        Some(&runtime::Funcopts { description: Some("OpenAI-compatible chat completion (explicit model)".into()), tags: vec!["network".into()], attributes: net }))?;
    Ok(())
}
