//! Email validation/parsing as DuckDB scalars (via `email_address`):
//!   email_validate -> bool, email_domain -> text, email_local -> text.
//! NULL / invalid -> NULL (email_validate -> false).
use std::collections::HashMap;
use std::str::FromStr;
use std::sync::{atomic::{AtomicU32, Ordering}, Mutex, OnceLock};
use wit_bindgen::rt::string::String;
use wit_bindgen::rt::vec::Vec;
wit_bindgen::generate!({ path: "./wit", world: "duckdb:extension/duckdb-extension" });
use duckdb::extension::{runtime, types};
use exports::duckdb::extension::{callback_dispatch, guest};
use email_address::EmailAddress;
struct Extension;
impl guest::Guest for Extension {
    fn load() -> Result<types::Loadresult, types::Duckerror> {
        register_scalars()?;
        Ok(types::Loadresult { name: "email".into(), version: Some(env!("CARGO_PKG_VERSION").into()), requires: Vec::new().into() })
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
        let s = arg(&args, 0);
        if which == H::Validate {
            return Ok(types::Duckvalue::Boolean(s.as_deref().map(EmailAddress::is_valid).unwrap_or(false)));
        }
        let addr = match s.as_deref().and_then(|s| EmailAddress::from_str(s).ok()) { Some(a) => a, None => return Ok(types::Duckvalue::Null) };
        Ok(match which {
            H::Domain => types::Duckvalue::Text(addr.domain().into()),
            H::Local => types::Duckvalue::Text(addr.local_part().into()),
            H::Validate => unreachable!(),
        })
    }
    fn call_table(_h: u32, _a: Vec<types::Duckvalue>) -> Result<types::Resultset, types::Duckerror> { Err(types::Duckerror::Unsupported("email: no table fns".into())) }
    fn call_aggregate(_h: u32, _r: types::Rowbatch) -> Result<types::Duckvalue, types::Duckerror> { Err(types::Duckerror::Unsupported("email: no aggs".into())) }
    fn call_pragma(_h: u32, _a: Vec<types::Duckvalue>) -> Result<Option<types::Duckvalue>, types::Duckerror> { Err(types::Duckerror::Unsupported("email: no pragmas".into())) }
    fn call_cast(_h: u32, _v: types::Duckvalue) -> Result<types::Duckvalue, types::Duckerror> { Err(types::Duckerror::Unsupported("email: no casts".into())) }
}
export!(Extension);
fn register_scalars() -> Result<(), types::Duckerror> {
    let cap = runtime::get_capability(types::Capabilitykind::Scalar).ok_or_else(|| types::Duckerror::Internal("no scalar capability".into()))?;
    let reg = match cap { runtime::Capability::Scalar(r) => r, _ => return Err(types::Duckerror::Internal("bad capability".into())) };
    let det = types::Funcflags::DETERMINISTIC | types::Funcflags::STATELESS;
    one(&reg, "email_validate", types::Logicaltype::Boolean, det, H::Validate)?;
    one(&reg, "email_domain", types::Logicaltype::Text, det, H::Domain)?;
    one(&reg, "email_local", types::Logicaltype::Text, det, H::Local)?;
    Ok(())
}
fn one(reg: &runtime::ScalarRegistry, name: &str, ret: types::Logicaltype, attr: types::Funcflags, h: H) -> Result<(), types::Duckerror> {
    let handle = NEXT.fetch_add(1, Ordering::Relaxed);
    handlers().lock().unwrap().insert(handle, h);
    let cb = runtime::ScalarCallback::new(handle);
    let args = vec![runtime::Funcarg { name: Some("email".into()), logical: types::Logicaltype::Text }];
    let opts = runtime::Funcopts { description: Some("email address".into()), tags: vec!["email".into()], attributes: attr };
    reg.register(name, &args, ret, cb, Some(&opts))?; Ok(())
}
#[derive(Clone, Copy, PartialEq)] enum H { Validate, Domain, Local }
static NEXT: AtomicU32 = AtomicU32::new(1);
static HANDLERS: OnceLock<Mutex<HashMap<u32, H>>> = OnceLock::new();
fn handlers() -> &'static Mutex<HashMap<u32, H>> { HANDLERS.get_or_init(|| Mutex::new(HashMap::new())) }
