//! Semantic-version parsing/compare as DuckDB scalars (via `semver`):
//!   semver_valid -> bool, semver_major/minor/patch -> bigint,
//!   semver_compare(a,b) -> bigint (-1/0/1). NULL/invalid -> NULL (valid->false).
use std::cmp::Ordering as Ord_;
use std::collections::HashMap;
use std::sync::{atomic::{AtomicU32, Ordering}, Mutex, OnceLock};
use wit_bindgen::rt::string::String;
use wit_bindgen::rt::vec::Vec;
wit_bindgen::generate!({ path: "./wit", world: "duckdb:extension/duckdb-extension" });
use duckdb::extension::{runtime, types};
use exports::duckdb::extension::{callback_dispatch, guest};
use semver::Version;
struct Extension;
impl guest::Guest for Extension {
    fn load() -> Result<types::Loadresult, types::Duckerror> {
        register_scalars()?;
        Ok(types::Loadresult { name: "semver".into(), version: Some(env!("CARGO_PKG_VERSION").into()), requires: Vec::new().into() })
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
        if which == H::Compare {
            let a = arg(&args, 0).and_then(|s| Version::parse(&s).ok());
            let b = arg(&args, 1).and_then(|s| Version::parse(&s).ok());
            return Ok(match (a, b) {
                (Some(a), Some(b)) => types::Duckvalue::Int64(match a.cmp(&b) { Ord_::Less => -1, Ord_::Equal => 0, Ord_::Greater => 1 }),
                _ => types::Duckvalue::Null,
            });
        }
        let s = arg(&args, 0);
        if which == H::Valid {
            return Ok(types::Duckvalue::Boolean(s.as_deref().map(|s| Version::parse(s).is_ok()).unwrap_or(false)));
        }
        let v = match s.and_then(|s| Version::parse(&s).ok()) { Some(v) => v, None => return Ok(types::Duckvalue::Null) };
        Ok(types::Duckvalue::Int64(match which {
            H::Major => v.major as i64, H::Minor => v.minor as i64, H::Patch => v.patch as i64,
            H::Valid | H::Compare => unreachable!(),
        }))
    }
    fn call_table(_h: u32, _a: Vec<types::Duckvalue>) -> Result<types::Resultset, types::Duckerror> { Err(types::Duckerror::Unsupported("semver: no table fns".into())) }
    fn call_aggregate(_h: u32, _r: types::Rowbatch) -> Result<types::Duckvalue, types::Duckerror> { Err(types::Duckerror::Unsupported("semver: no aggs".into())) }
    fn call_pragma(_h: u32, _a: Vec<types::Duckvalue>) -> Result<Option<types::Duckvalue>, types::Duckerror> { Err(types::Duckerror::Unsupported("semver: no pragmas".into())) }
    fn call_cast(_h: u32, _v: types::Duckvalue) -> Result<types::Duckvalue, types::Duckerror> { Err(types::Duckerror::Unsupported("semver: no casts".into())) }
}
export!(Extension);
fn register_scalars() -> Result<(), types::Duckerror> {
    let cap = runtime::get_capability(types::Capabilitykind::Scalar).ok_or_else(|| types::Duckerror::Internal("no scalar capability".into()))?;
    let reg = match cap { runtime::Capability::Scalar(r) => r, _ => return Err(types::Duckerror::Internal("bad capability".into())) };
    let det = types::Funcflags::DETERMINISTIC | types::Funcflags::STATELESS;
    one1(&reg, "semver_valid", types::Logicaltype::Boolean, det, H::Valid)?;
    one1(&reg, "semver_major", types::Logicaltype::Int64, det, H::Major)?;
    one1(&reg, "semver_minor", types::Logicaltype::Int64, det, H::Minor)?;
    one1(&reg, "semver_patch", types::Logicaltype::Int64, det, H::Patch)?;
    one2(&reg, "semver_compare", types::Logicaltype::Int64, det, H::Compare)?;
    Ok(())
}
fn register(reg: &runtime::ScalarRegistry, name: &str, args: &[runtime::Funcarg], ret: types::Logicaltype, attr: types::Funcflags, h: H) -> Result<(), types::Duckerror> {
    let handle = NEXT.fetch_add(1, Ordering::Relaxed);
    handlers().lock().unwrap().insert(handle, h);
    let cb = runtime::ScalarCallback::new(handle);
    let opts = runtime::Funcopts { description: Some("semver".into()), tags: vec!["semver".into()], attributes: attr };
    reg.register(name, args, ret, cb, Some(&opts))?; Ok(())
}
fn one1(reg: &runtime::ScalarRegistry, name: &str, ret: types::Logicaltype, attr: types::Funcflags, h: H) -> Result<(), types::Duckerror> {
    register(reg, name, &[runtime::Funcarg { name: Some("v".into()), logical: types::Logicaltype::Text }], ret, attr, h)
}
fn one2(reg: &runtime::ScalarRegistry, name: &str, ret: types::Logicaltype, attr: types::Funcflags, h: H) -> Result<(), types::Duckerror> {
    register(reg, name, &[
        runtime::Funcarg { name: Some("a".into()), logical: types::Logicaltype::Text },
        runtime::Funcarg { name: Some("b".into()), logical: types::Logicaltype::Text },
    ], ret, attr, h)
}
#[derive(Clone, Copy, PartialEq)] enum H { Valid, Major, Minor, Patch, Compare }
static NEXT: AtomicU32 = AtomicU32::new(1);
static HANDLERS: OnceLock<Mutex<HashMap<u32, H>>> = OnceLock::new();
fn handlers() -> &'static Mutex<HashMap<u32, H>> { HANDLERS.get_or_init(|| Mutex::new(HashMap::new())) }
