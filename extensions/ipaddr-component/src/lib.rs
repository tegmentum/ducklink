//! IP address validation + classification as DuckDB scalars (std::net):
//!   ip_valid(text) -> bool, ip_version(text) -> bigint (4 or 6; NULL if
//!   invalid), ip_is_private(text) -> bool (RFC 1918 v4 / unique-local v6).
//!   NULL -> NULL (ip_valid -> false).
use std::collections::HashMap;
use std::net::{IpAddr, Ipv6Addr};
use std::str::FromStr;
use std::sync::{atomic::{AtomicU32, Ordering}, Mutex, OnceLock};
use wit_bindgen::rt::string::String;
use wit_bindgen::rt::vec::Vec;
wit_bindgen::generate!({ path: "./wit", world: "duckdb:extension/duckdb-extension" });
use duckdb::extension::{runtime, types};
use exports::duckdb::extension::{callback_dispatch, guest};
struct Extension;
impl guest::Guest for Extension {
    fn load() -> Result<types::Loadresult, types::Duckerror> {
        register_scalars()?;
        Ok(types::Loadresult { name: "ipaddr".into(), version: Some(env!("CARGO_PKG_VERSION").into()), requires: Vec::new().into() })
    }
    fn reconfigure(_k: Vec<String>) -> Result<bool, types::Duckerror> { Ok(false) }
    fn shutdown() -> Result<bool, types::Duckerror> { Ok(false) }
}
fn parse(args: &[types::Duckvalue]) -> Option<IpAddr> {
    match args.first() { Some(types::Duckvalue::Text(s)) => IpAddr::from_str(s.trim()).ok(), _ => None }
}
/// RFC 4193 unique-local addresses (fc00::/7); std has no stable predicate.
fn is_unique_local(a: &Ipv6Addr) -> bool { (a.segments()[0] & 0xfe00) == 0xfc00 }
impl callback_dispatch::Guest for Extension {
    fn call_scalar_batch(h: u32, rows: Vec<Vec<types::Duckvalue>>, ctx: types::Invokeinfo) -> Result<Vec<types::Duckvalue>, types::Duckerror> {
        let base = ctx.rowindex.unwrap_or(0); let mut out = Vec::with_capacity(rows.len());
        for (i, a) in rows.into_iter().enumerate() {
            out.push(Self::call_scalar(h, a, types::Invokeinfo { rowindex: Some(base + i as u64), iswindow: ctx.iswindow })?);
        }
        Ok(out)
    }
    fn call_scalar(handle: u32, args: Vec<types::Duckvalue>, _c: types::Invokeinfo) -> Result<types::Duckvalue, types::Duckerror> {
        let which = handlers().lock().unwrap().get(&handle).copied()
            .ok_or_else(|| types::Duckerror::Internal("unknown scalar handle".into()))?;
        let ip = parse(&args);
        Ok(match which {
            I::Valid => types::Duckvalue::Boolean(ip.is_some()),
            I::Version => match ip { Some(IpAddr::V4(_)) => types::Duckvalue::Int64(4), Some(IpAddr::V6(_)) => types::Duckvalue::Int64(6), None => types::Duckvalue::Null },
            I::Private => match ip {
                Some(IpAddr::V4(a)) => types::Duckvalue::Boolean(a.is_private()),
                Some(IpAddr::V6(a)) => types::Duckvalue::Boolean(is_unique_local(&a)),
                None => types::Duckvalue::Boolean(false),
            },
        })
    }
    fn call_table(_h: u32, _a: Vec<types::Duckvalue>) -> Result<types::Resultset, types::Duckerror> { Err(types::Duckerror::Unsupported("ipaddr: no table fns".into())) }
    fn call_aggregate(_h: u32, _r: types::Rowbatch) -> Result<types::Duckvalue, types::Duckerror> { Err(types::Duckerror::Unsupported("ipaddr: no aggs".into())) }
    fn call_pragma(_h: u32, _a: Vec<types::Duckvalue>) -> Result<Option<types::Duckvalue>, types::Duckerror> { Err(types::Duckerror::Unsupported("ipaddr: no pragmas".into())) }
    fn call_cast(_h: u32, _v: types::Duckvalue) -> Result<types::Duckvalue, types::Duckerror> { Err(types::Duckerror::Unsupported("ipaddr: no casts".into())) }
}
export!(Extension);
fn register_scalars() -> Result<(), types::Duckerror> {
    let cap = runtime::get_capability(types::Capabilitykind::Scalar).ok_or_else(|| types::Duckerror::Internal("no scalar capability".into()))?;
    let reg = match cap { runtime::Capability::Scalar(r) => r, _ => return Err(types::Duckerror::Internal("bad capability".into())) };
    let det = types::Funcflags::DETERMINISTIC | types::Funcflags::STATELESS;
    one(&reg, "ip_valid", types::Logicaltype::Boolean, det, I::Valid)?;
    one(&reg, "ip_version", types::Logicaltype::Int64, det, I::Version)?;
    one(&reg, "ip_is_private", types::Logicaltype::Boolean, det, I::Private)?;
    Ok(())
}
fn one(reg: &runtime::ScalarRegistry, name: &str, ret: types::Logicaltype, attr: types::Funcflags, i: I) -> Result<(), types::Duckerror> {
    let h = NEXT.fetch_add(1, Ordering::Relaxed); handlers().lock().unwrap().insert(h, i);
    reg.register(name, &[runtime::Funcarg { name: Some("ip".into()), logical: types::Logicaltype::Text }],
        ret, runtime::ScalarCallback::new(h),
        Some(&runtime::Funcopts { description: Some("IP address".into()), tags: vec!["network".into()], attributes: attr }))?;
    Ok(())
}
#[derive(Clone, Copy)] enum I { Valid, Version, Private }
static NEXT: AtomicU32 = AtomicU32::new(1);
static HANDLERS: OnceLock<Mutex<HashMap<u32, I>>> = OnceLock::new();
fn handlers() -> &'static Mutex<HashMap<u32, I>> { HANDLERS.get_or_init(|| Mutex::new(HashMap::new())) }
