//! URL parsing as DuckDB scalars (via the `url` crate):
//!   url_scheme / url_host / url_port / url_path / url_query
//! NULL / unparseable -> NULL.
use std::collections::HashMap;
use std::sync::{atomic::{AtomicU32, Ordering}, Mutex, OnceLock};
use wit_bindgen::rt::string::String;
use wit_bindgen::rt::vec::Vec;
wit_bindgen::generate!({ path: "./wit", world: "duckdb:extension/duckdb-extension" });
use duckdb::extension::{runtime, types};
use exports::duckdb::extension::{callback_dispatch, guest};
use url::Url;
struct Extension;
impl guest::Guest for Extension {
    fn load() -> Result<types::Loadresult, types::Duckerror> {
        register_scalars()?;
        Ok(types::Loadresult { name: "url".into(), version: Some(env!("CARGO_PKG_VERSION").into()), requires: Vec::new().into() })
    }
    fn reconfigure(_k: Vec<String>) -> Result<bool, types::Duckerror> { Ok(false) }
    fn shutdown() -> Result<bool, types::Duckerror> { Ok(false) }
}
fn arg(args: &[types::Duckvalue], i: usize) -> Option<String> {
    match args.get(i) { Some(types::Duckvalue::Text(s)) => Some(s.clone()), _ => None }
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
        let which = handlers().lock().unwrap().get(&handle).copied()
            .ok_or_else(|| types::Duckerror::Internal("unknown scalar handle".into()))?;
        let u = match arg(&args, 0).and_then(|s| Url::parse(&s).ok()) { Some(u) => u, None => return Ok(types::Duckvalue::Null) };
        Ok(match which {
            H::Scheme => types::Duckvalue::Text(u.scheme().into()),
            H::Host => match u.host_str() { Some(h) => types::Duckvalue::Text(h.into()), None => types::Duckvalue::Null },
            H::Port => match u.port_or_known_default() { Some(p) => types::Duckvalue::Int64(p as i64), None => types::Duckvalue::Null },
            H::Path => types::Duckvalue::Text(u.path().into()),
            H::Query => match u.query() { Some(q) => types::Duckvalue::Text(q.into()), None => types::Duckvalue::Null },
        })
    }
    fn call_table(_h: u32, _a: Vec<types::Duckvalue>) -> Result<types::Resultset, types::Duckerror> { Err(types::Duckerror::Unsupported("url: no table fns".into())) }
    fn call_aggregate(_h: u32, _r: types::Rowbatch) -> Result<types::Duckvalue, types::Duckerror> { Err(types::Duckerror::Unsupported("url: no aggs".into())) }
    fn call_pragma(_h: u32, _a: Vec<types::Duckvalue>) -> Result<Option<types::Duckvalue>, types::Duckerror> { Err(types::Duckerror::Unsupported("url: no pragmas".into())) }
    fn call_cast(_h: u32, _v: types::Duckvalue) -> Result<types::Duckvalue, types::Duckerror> { Err(types::Duckerror::Unsupported("url: no casts".into())) }
}
export!(Extension);
fn register_scalars() -> Result<(), types::Duckerror> {
    let cap = runtime::get_capability(types::Capabilitykind::Scalar).ok_or_else(|| types::Duckerror::Internal("no scalar capability".into()))?;
    let reg = match cap { runtime::Capability::Scalar(r) => r, _ => return Err(types::Duckerror::Internal("bad capability".into())) };
    let det = types::Funcflags::DETERMINISTIC | types::Funcflags::STATELESS;
    one(&reg, "url_scheme", types::Logicaltype::Text, det, H::Scheme)?;
    one(&reg, "url_host", types::Logicaltype::Text, det, H::Host)?;
    one(&reg, "url_port", types::Logicaltype::Int64, det, H::Port)?;
    one(&reg, "url_path", types::Logicaltype::Text, det, H::Path)?;
    one(&reg, "url_query", types::Logicaltype::Text, det, H::Query)?;
    Ok(())
}
fn one(reg: &runtime::ScalarRegistry, name: &str, ret: types::Logicaltype, attr: types::Funcflags, h: H) -> Result<(), types::Duckerror> {
    let handle = NEXT.fetch_add(1, Ordering::Relaxed);
    handlers().lock().unwrap().insert(handle, h);
    let cb = runtime::ScalarCallback::new(handle);
    let args = vec![runtime::Funcarg { name: Some("url".into()), logical: types::Logicaltype::Text }];
    let opts = runtime::Funcopts { description: Some("URL component".into()), tags: vec!["url".into()], attributes: attr };
    reg.register(name, &args, &ret, cb, Some(&opts))?; Ok(())
}
#[derive(Clone, Copy)] enum H { Scheme, Host, Port, Path, Query }
static NEXT: AtomicU32 = AtomicU32::new(1);
static HANDLERS: OnceLock<Mutex<HashMap<u32, H>>> = OnceLock::new();
fn handlers() -> &'static Mutex<HashMap<u32, H>> { HANDLERS.get_or_init(|| Mutex::new(HashMap::new())) }
