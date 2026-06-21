//! Identifier generators as DuckDB scalars:
//!   ulid() -> text, nanoid() -> text (both non-deterministic; need wasi
//!   random + clock), ulid_timestamp(text) -> bigint (ms since epoch; NULL if
//!   not a valid ULID). ULID + nanoid via the `ulid` / `nanoid` crates.
use std::collections::HashMap;
use std::str::FromStr;
use std::sync::{atomic::{AtomicU32, Ordering}, Mutex, OnceLock};
use wit_bindgen::rt::string::String;
use wit_bindgen::rt::vec::Vec;
wit_bindgen::generate!({ path: "./wit", world: "duckdb:extension/duckdb-extension" });
use duckdb::extension::{runtime, types};
use exports::duckdb::extension::{callback_dispatch, guest};
use ulid::Ulid;
struct Extension;
impl guest::Guest for Extension {
    fn load() -> Result<types::Loadresult, types::Duckerror> {
        register_scalars()?;
        Ok(types::Loadresult { name: "ids".into(), version: Some(env!("CARGO_PKG_VERSION").into()), requires: Vec::new().into() })
    }
    fn reconfigure(_k: Vec<String>) -> Result<bool, types::Duckerror> { Ok(false) }
    fn shutdown() -> Result<bool, types::Duckerror> { Ok(false) }
}
fn text_arg(args: &[types::Duckvalue], i: usize) -> Option<String> {
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
        Ok(match which {
            H::Ulid => types::Duckvalue::Text(Ulid::new().to_string().into()),
            H::NanoId => types::Duckvalue::Text(nanoid::nanoid!().into()),
            H::UlidTs => match text_arg(&args, 0).and_then(|s| Ulid::from_str(&s).ok()) {
                Some(u) => types::Duckvalue::Int64(u.timestamp_ms() as i64), None => types::Duckvalue::Null },
        })
    }
    fn call_table(_h: u32, _a: Vec<types::Duckvalue>) -> Result<types::Resultset, types::Duckerror> { Err(types::Duckerror::Unsupported("ids: no table fns".into())) }
    fn call_aggregate(_h: u32, _r: types::Rowbatch) -> Result<types::Duckvalue, types::Duckerror> { Err(types::Duckerror::Unsupported("ids: no aggs".into())) }
    fn call_pragma(_h: u32, _a: Vec<types::Duckvalue>) -> Result<Option<types::Duckvalue>, types::Duckerror> { Err(types::Duckerror::Unsupported("ids: no pragmas".into())) }
    fn call_cast(_h: u32, _v: types::Duckvalue) -> Result<types::Duckvalue, types::Duckerror> { Err(types::Duckerror::Unsupported("ids: no casts".into())) }
}
export!(Extension);
fn register_scalars() -> Result<(), types::Duckerror> {
    let cap = runtime::get_capability(types::Capabilitykind::Scalar).ok_or_else(|| types::Duckerror::Internal("no scalar capability".into()))?;
    let reg = match cap { runtime::Capability::Scalar(r) => r, _ => return Err(types::Duckerror::Internal("bad capability".into())) };
    let det = types::Funcflags::DETERMINISTIC | types::Funcflags::STATELESS;
    let nondet = types::Funcflags::empty();
    reg0(&reg, "ulid", types::Logicaltype::Text, nondet, H::Ulid)?;
    reg0(&reg, "nanoid", types::Logicaltype::Text, nondet, H::NanoId)?;
    let h = NEXT.fetch_add(1, Ordering::Relaxed); handlers().lock().unwrap().insert(h, H::UlidTs);
    reg.register("ulid_timestamp",
        &[runtime::Funcarg { name: Some("ulid".into()), logical: types::Logicaltype::Text }],
        types::Logicaltype::Int64, runtime::ScalarCallback::new(h),
        Some(&runtime::Funcopts { description: Some("ULID -> epoch ms".into()), tags: vec!["id".into()], attributes: det }))?;
    Ok(())
}
fn reg0(reg: &runtime::ScalarRegistry, name: &str, ret: types::Logicaltype, attr: types::Funcflags, h: H) -> Result<(), types::Duckerror> {
    let handle = NEXT.fetch_add(1, Ordering::Relaxed); handlers().lock().unwrap().insert(handle, h);
    reg.register(name, &[], ret, runtime::ScalarCallback::new(handle),
        Some(&runtime::Funcopts { description: Some("identifier generator".into()), tags: vec!["id".into()], attributes: attr }))?;
    Ok(())
}
#[derive(Clone, Copy)] enum H { Ulid, NanoId, UlidTs }
static NEXT: AtomicU32 = AtomicU32::new(1);
static HANDLERS: OnceLock<Mutex<HashMap<u32, H>>> = OnceLock::new();
fn handlers() -> &'static Mutex<HashMap<u32, H>> { HANDLERS.get_or_init(|| Mutex::new(HashMap::new())) }
