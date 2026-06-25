//! Namespace UUIDs as DuckDB scalars (via `uuid`): uuid_v5(namespace, name) and
//! uuid_v3(namespace, name). `namespace` is a UUID string or a well-known alias
//! (dns / url / oid / x500). Deterministic. Bad namespace / NULL -> NULL.
use std::collections::HashMap;
use std::sync::{atomic::{AtomicU32, Ordering}, Mutex, OnceLock};
use uuid::Uuid;
use wit_bindgen::rt::string::String;
use wit_bindgen::rt::vec::Vec;
wit_bindgen::generate!({ path: "./wit", world: "duckdb:extension/duckdb-extension" });
use duckdb::extension::{runtime, types};
use exports::duckdb::extension::{callback_dispatch, guest};
struct Extension;
impl guest::Guest for Extension {
    fn load() -> Result<types::Loadresult, types::Duckerror> {
        register_scalars()?;
        Ok(types::Loadresult { name: "uuid5".into(), version: Some(env!("CARGO_PKG_VERSION").into()), requires: Vec::new().into() })
    }
    fn reconfigure(_k: Vec<String>) -> Result<bool, types::Duckerror> { Ok(false) }
    fn shutdown() -> Result<bool, types::Duckerror> { Ok(false) }
}
fn text_arg(args: &[types::Duckvalue], i: usize) -> Option<String> {
    match args.get(i) { Some(types::Duckvalue::Text(s)) => Some(s.clone()), _ => None }
}
fn namespace(s: &str) -> Option<Uuid> {
    match s.trim().to_ascii_lowercase().as_str() {
        "dns" => Some(Uuid::NAMESPACE_DNS), "url" => Some(Uuid::NAMESPACE_URL),
        "oid" => Some(Uuid::NAMESPACE_OID), "x500" => Some(Uuid::NAMESPACE_X500),
        _ => Uuid::parse_str(s.trim()).ok(),
    }
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
        let v3 = handle == 2;
        let ns = match text_arg(&args, 0).and_then(|s| namespace(&s)) { Some(n) => n, None => return Ok(types::Duckvalue::Null) };
        let name = match text_arg(&args, 1) { Some(s) => s, None => return Ok(types::Duckvalue::Null) };
        let u = if v3 { Uuid::new_v3(&ns, name.as_bytes()) } else { Uuid::new_v5(&ns, name.as_bytes()) };
        Ok(types::Duckvalue::Text(u.to_string().into()))
    }
    fn call_table(_h: u32, _a: Vec<types::Duckvalue>) -> Result<types::Resultset, types::Duckerror> { Err(types::Duckerror::Unsupported("uuid5: no table fns".into())) }
    fn call_aggregate(_h: u32, _r: types::Rowbatch) -> Result<types::Duckvalue, types::Duckerror> { Err(types::Duckerror::Unsupported("uuid5: no aggs".into())) }
    fn call_pragma(_h: u32, _a: Vec<types::Duckvalue>) -> Result<Option<types::Duckvalue>, types::Duckerror> { Err(types::Duckerror::Unsupported("uuid5: no pragmas".into())) }
    fn call_cast(_h: u32, _v: types::Duckvalue) -> Result<types::Duckvalue, types::Duckerror> { Err(types::Duckerror::Unsupported("uuid5: no casts".into())) }
}
export!(Extension);
fn register_scalars() -> Result<(), types::Duckerror> {
    let cap = runtime::get_capability(types::Capabilitykind::Scalar).ok_or_else(|| types::Duckerror::Internal("no scalar capability".into()))?;
    let reg = match cap { runtime::Capability::Scalar(r) => r, _ => return Err(types::Duckerror::Internal("bad capability".into())) };
    let det = types::Funcflags::DETERMINISTIC | types::Funcflags::STATELESS;
    for (name, cb) in [("uuid_v5", 1u32), ("uuid_v3", 2u32)] {
        reg.register(name, &[
            runtime::Funcarg { name: Some("namespace".into()), logical: types::Logicaltype::Text },
            runtime::Funcarg { name: Some("name".into()), logical: types::Logicaltype::Text }],
            &types::Logicaltype::Text, runtime::ScalarCallback::new(cb),
            Some(&runtime::Funcopts { description: Some("namespace UUID".into()), tags: vec!["uuid".into()], attributes: det }))?;
    }
    Ok(())
}
