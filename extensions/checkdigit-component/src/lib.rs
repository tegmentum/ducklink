//! Check-digit algorithms as DuckDB scalars (hand-rolled): the Verhoeff and Damm
//! schemes, which (unlike Luhn) catch all single-digit and adjacent-transposition
//! errors. verhoeff_validate / verhoeff_append / damm_validate / damm_append.
//! Non-digits are stripped; empty -> NULL/false.
use std::collections::HashMap;
use std::sync::{atomic::{AtomicU32, Ordering}, Mutex, OnceLock};
use wit_bindgen::rt::string::String;
use wit_bindgen::rt::vec::Vec;
wit_bindgen::generate!({ path: "./wit", world: "duckdb:extension/duckdb-extension" });
use duckdb::extension::{runtime, types};
use exports::duckdb::extension::{callback_dispatch, guest};
struct Extension;
const D: [[u8; 10]; 10] = [
    [0,1,2,3,4,5,6,7,8,9],[1,2,3,4,0,6,7,8,9,5],[2,3,4,0,1,7,8,9,5,6],[3,4,0,1,2,8,9,5,6,7],
    [4,0,1,2,3,9,5,6,7,8],[5,9,8,7,6,0,4,3,2,1],[6,5,9,8,7,1,0,4,3,2],[7,6,5,9,8,2,1,0,4,3],
    [8,7,6,5,9,3,2,1,0,4],[9,8,7,6,5,4,3,2,1,0]];
const P: [[u8; 10]; 8] = [
    [0,1,2,3,4,5,6,7,8,9],[1,5,7,6,2,8,3,0,9,4],[5,8,0,3,7,9,6,1,4,2],[8,9,1,6,0,4,3,5,2,7],
    [9,4,5,3,1,2,6,8,7,0],[4,2,8,6,5,7,3,9,0,1],[2,7,9,3,8,0,6,4,1,5],[7,0,4,6,9,1,3,2,5,8]];
const INV: [u8; 10] = [0,4,3,2,1,5,6,7,8,9];
const DAMM: [[u8; 10]; 10] = [
    [0,3,1,7,5,9,8,6,4,2],[7,0,9,2,1,5,4,8,6,3],[4,2,0,6,8,7,1,3,5,9],[1,7,5,0,9,8,3,4,2,6],
    [6,1,2,3,0,4,5,9,7,8],[3,6,7,4,2,0,9,5,8,1],[5,8,6,9,7,2,0,1,3,4],[8,9,4,5,3,6,2,0,1,7],
    [9,4,3,8,6,1,7,2,0,5],[2,5,8,1,4,3,6,7,9,0]];
fn digits(s: &str) -> std::vec::Vec<u8> {
    s.chars().filter_map(|c| c.to_digit(10).map(|d| d as u8)).collect()
}
fn verhoeff_check(ds: &[u8], for_append: bool) -> u8 {
    let mut c = 0u8;
    for (i, &d) in ds.iter().rev().enumerate() {
        let row = if for_append { (i + 1) % 8 } else { i % 8 };
        c = D[c as usize][P[row][d as usize] as usize];
    }
    c
}
fn damm_interim(ds: &[u8]) -> u8 {
    let mut interim = 0u8;
    for &d in ds { interim = DAMM[interim as usize][d as usize]; }
    interim
}
impl guest::Guest for Extension {
    fn load() -> Result<types::Loadresult, types::Duckerror> {
        register_scalars()?;
        Ok(types::Loadresult { name: "checkdigit".into(), version: Some(env!("CARGO_PKG_VERSION").into()), requires: Vec::new().into() })
    }
    fn reconfigure(_k: Vec<String>) -> Result<bool, types::Duckerror> { Ok(false) }
    fn shutdown() -> Result<bool, types::Duckerror> { Ok(false) }
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
        let raw = match args.first() { Some(types::Duckvalue::Text(s)) => s.clone(), _ => return Ok(types::Duckvalue::Null) };
        let ds = digits(&raw);
        if ds.is_empty() { return Ok(match which { K::VVal | K::DVal => types::Duckvalue::Boolean(false), _ => types::Duckvalue::Null }); }
        Ok(match which {
            K::VVal => types::Duckvalue::Boolean(verhoeff_check(&ds, false) == 0),
            K::VApp => { let cd = INV[verhoeff_check(&ds, true) as usize]; types::Duckvalue::Text(format!("{}{}", to_str(&ds), cd).into()) }
            K::DVal => types::Duckvalue::Boolean(damm_interim(&ds) == 0),
            K::DApp => { let cd = damm_interim(&ds); types::Duckvalue::Text(format!("{}{}", to_str(&ds), cd).into()) }
        })
    }
    fn call_table(_h: u32, _a: Vec<types::Duckvalue>) -> Result<types::Resultset, types::Duckerror> { Err(types::Duckerror::Unsupported("checkdigit: no table fns".into())) }
    fn call_aggregate(_h: u32, _r: types::Rowbatch) -> Result<types::Duckvalue, types::Duckerror> { Err(types::Duckerror::Unsupported("checkdigit: no aggs".into())) }
    fn call_pragma(_h: u32, _a: Vec<types::Duckvalue>) -> Result<Option<types::Duckvalue>, types::Duckerror> { Err(types::Duckerror::Unsupported("checkdigit: no pragmas".into())) }
    fn call_cast(_h: u32, _v: types::Duckvalue) -> Result<types::Duckvalue, types::Duckerror> { Err(types::Duckerror::Unsupported("checkdigit: no casts".into())) }
}
fn to_str(ds: &[u8]) -> std::string::String { ds.iter().map(|d| (b'0' + d) as char).collect() }
export!(Extension);
fn register_scalars() -> Result<(), types::Duckerror> {
    let cap = runtime::get_capability(types::Capabilitykind::Scalar).ok_or_else(|| types::Duckerror::Internal("no scalar capability".into()))?;
    let reg = match cap { runtime::Capability::Scalar(r) => r, _ => return Err(types::Duckerror::Internal("bad capability".into())) };
    let det = types::Funcflags::DETERMINISTIC | types::Funcflags::STATELESS;
    one(&reg, "verhoeff_validate", types::Logicaltype::Boolean, det, K::VVal)?;
    one(&reg, "verhoeff_append", types::Logicaltype::Text, det, K::VApp)?;
    one(&reg, "damm_validate", types::Logicaltype::Boolean, det, K::DVal)?;
    one(&reg, "damm_append", types::Logicaltype::Text, det, K::DApp)?;
    Ok(())
}
fn one(reg: &runtime::ScalarRegistry, name: &str, ret: types::Logicaltype, attr: types::Funcflags, k: K) -> Result<(), types::Duckerror> {
    let h = NEXT.fetch_add(1, Ordering::Relaxed); handlers().lock().unwrap().insert(h, k);
    reg.register(name, &[runtime::Funcarg { name: Some("number".into()), logical: types::Logicaltype::Text }],
        ret, runtime::ScalarCallback::new(h),
        Some(&runtime::Funcopts { description: Some("check digit".into()), tags: vec!["validation".into()], attributes: attr }))?;
    Ok(())
}
#[derive(Clone, Copy)] enum K { VVal, VApp, DVal, DApp }
static NEXT: AtomicU32 = AtomicU32::new(1);
static HANDLERS: OnceLock<Mutex<HashMap<u32, K>>> = OnceLock::new();
fn handlers() -> &'static Mutex<HashMap<u32, K>> { HANDLERS.get_or_init(|| Mutex::new(HashMap::new())) }
