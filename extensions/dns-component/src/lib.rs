//! DNS resolution as DuckDB scalars over the wasi:sockets graft (the host grants
//! extension components outbound network + name lookup):
//!   dns_lookup(host) -> text (first resolved IP), dns_resolve_all(host) -> text
//!   (JSON array of IPs). Nondeterministic (network). Unresolvable -> NULL.
use std::net::ToSocketAddrs;
use wit_bindgen::rt::string::String;
use wit_bindgen::rt::vec::Vec;
wit_bindgen::generate!({ path: "./wit", world: "duckdb:extension/duckdb-extension" });
use duckdb::extension::{runtime, types};
use exports::duckdb::extension::{callback_dispatch, guest};
struct Extension;
impl guest::Guest for Extension {
    fn load() -> Result<types::Loadresult, types::Duckerror> {
        register_scalars()?;
        Ok(types::Loadresult { name: "dns".into(), version: Some(env!("CARGO_PKG_VERSION").into()), requires: Vec::new().into() })
    }
    fn reconfigure(_k: Vec<String>) -> Result<bool, types::Duckerror> { Ok(false) }
    fn shutdown() -> Result<bool, types::Duckerror> { Ok(false) }
}
fn host_arg(args: &[types::Duckvalue]) -> Option<String> {
    match args.first() { Some(types::Duckvalue::Text(s)) => Some(s.clone()), _ => None }
}
fn resolve(host: &str) -> Option<std::vec::Vec<std::string::String>> {
    let host = host.trim();
    if host.is_empty() { return None; }
    let mut ips: std::vec::Vec<std::string::String> = (host, 0u16).to_socket_addrs().ok()?
        .map(|sa| sa.ip().to_string()).collect();
    ips.dedup();
    if ips.is_empty() { None } else { Some(ips) }
}
impl callback_dispatch::Guest for Extension {
    // major-4 columnar dispatch: dns is nondeterministic network I/O, so the
    // columnar hot path is Unsupported and the host falls back to the per-row
    // call_scalar below (unchanged hand-written logic).
    datalink_extcore::columnar_stub!();
    fn call_scalar(handle: u32, args: Vec<types::Duckvalue>, _c: types::Invokeinfo) -> Result<types::Duckvalue, types::Duckerror> {
        let host = match host_arg(&args) { Some(s) => s, None => return Ok(types::Duckvalue::Null) };
        let ips = match resolve(&host) { Some(v) => v, None => return Ok(types::Duckvalue::Null) };
        Ok(if handle == 1 {
            types::Duckvalue::Text(ips.into_iter().next().unwrap().into())
        } else {
            let body = ips.iter().map(|ip| format!("\"{}\"", ip)).collect::<std::vec::Vec<_>>().join(",");
            types::Duckvalue::Text(format!("[{}]", body).into())
        })
    }
    fn call_table(_h: u32, _a: Vec<types::Duckvalue>) -> Result<types::Resultset, types::Duckerror> { Err(types::Duckerror::Unsupported("dns: no table fns".into())) }
    fn call_pragma(_h: u32, _a: Vec<types::Duckvalue>) -> Result<Option<types::Duckvalue>, types::Duckerror> { Err(types::Duckerror::Unsupported("dns: no pragmas".into())) }
    fn call_cast(_h: u32, _v: types::Duckvalue) -> Result<types::Duckvalue, types::Duckerror> { Err(types::Duckerror::Unsupported("dns: no casts".into())) }
}
export!(Extension);
fn register_scalars() -> Result<(), types::Duckerror> {
    let cap = runtime::get_capability(types::Capabilitykind::Scalar).ok_or_else(|| types::Duckerror::Internal("no scalar capability".into()))?;
    let reg = match cap { runtime::Capability::Scalar(r) => r, _ => return Err(types::Duckerror::Internal("bad capability".into())) };
    let net = types::Funcflags::empty();
    reg.register("dns_lookup", &[runtime::Funcarg { name: Some("host".into()), logical: types::Logicaltype::Text }],
        &types::Logicaltype::Text, runtime::ScalarCallback::new(1),
        Some(&runtime::Funcopts { description: Some("resolve host -> IP".into()), tags: vec!["network".into()], attributes: net }))?;
    reg.register("dns_resolve_all", &[runtime::Funcarg { name: Some("host".into()), logical: types::Logicaltype::Text }],
        &types::Logicaltype::Text, runtime::ScalarCallback::new(2),
        Some(&runtime::Funcopts { description: Some("resolve host -> all IPs (JSON)".into()), tags: vec!["network".into()], attributes: net }))?;
    Ok(())
}
