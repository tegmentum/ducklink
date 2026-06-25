//! RPG dice notation as DuckDB scalars (via `rand`):
//!   dice_roll(notation) -> bigint (random; e.g. "2d6+3"), dice_min(notation),
//!   dice_max(notation) (deterministic bounds). Bad notation / NULL -> NULL.
use rand::Rng;
use wit_bindgen::rt::string::String;
use wit_bindgen::rt::vec::Vec;
wit_bindgen::generate!({ path: "./wit", world: "duckdb:extension/duckdb-extension" });
use duckdb::extension::{runtime, types};
use exports::duckdb::extension::{callback_dispatch, guest};
struct Extension;
impl guest::Guest for Extension {
    fn load() -> Result<types::Loadresult, types::Duckerror> {
        register_scalars()?;
        Ok(types::Loadresult { name: "dice".into(), version: Some(env!("CARGO_PKG_VERSION").into()), requires: Vec::new().into() })
    }
    fn reconfigure(_k: Vec<String>) -> Result<bool, types::Duckerror> { Ok(false) }
    fn shutdown() -> Result<bool, types::Duckerror> { Ok(false) }
}
/// Parse "[N]dM[(+|-)K]" into (count, sides, modifier).
fn parse(s: &str) -> Option<(i64, i64, i64)> {
    let s: std::string::String = s.chars().filter(|c| !c.is_whitespace()).collect::<std::string::String>().to_ascii_lowercase();
    let (cnt, rest) = s.split_once('d')?;
    let count: i64 = if cnt.is_empty() { 1 } else { cnt.parse().ok()? };
    let (sides, modifier) = match rest.find(['+', '-']) {
        Some(i) => (rest[..i].parse().ok()?, rest[i..].parse().ok()?),
        None => (rest.parse().ok()?, 0),
    };
    if count < 1 || count > 1000 || sides < 1 { return None; }
    Some((count, sides, modifier))
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
        let notation = match args.first() { Some(types::Duckvalue::Text(s)) => s.clone(), _ => return Ok(types::Duckvalue::Null) };
        let (count, sides, modifier) = match parse(&notation) { Some(t) => t, None => return Ok(types::Duckvalue::Null) };
        Ok(types::Duckvalue::Int64(match handle {
            2 => count + modifier,                 // min: all ones
            3 => count * sides + modifier,         // max
            _ => {                                  // roll
                let mut rng = rand::thread_rng();
                let sum: i64 = (0..count).map(|_| rng.gen_range(1..=sides)).sum();
                sum + modifier
            }
        }))
    }
    fn call_table(_h: u32, _a: Vec<types::Duckvalue>) -> Result<types::Resultset, types::Duckerror> { Err(types::Duckerror::Unsupported("dice: no table fns".into())) }
    fn call_aggregate(_h: u32, _r: types::Rowbatch) -> Result<types::Duckvalue, types::Duckerror> { Err(types::Duckerror::Unsupported("dice: no aggs".into())) }
    fn call_pragma(_h: u32, _a: Vec<types::Duckvalue>) -> Result<Option<types::Duckvalue>, types::Duckerror> { Err(types::Duckerror::Unsupported("dice: no pragmas".into())) }
    fn call_cast(_h: u32, _v: types::Duckvalue) -> Result<types::Duckvalue, types::Duckerror> { Err(types::Duckerror::Unsupported("dice: no casts".into())) }
}
export!(Extension);
fn register_scalars() -> Result<(), types::Duckerror> {
    let cap = runtime::get_capability(types::Capabilitykind::Scalar).ok_or_else(|| types::Duckerror::Internal("no scalar capability".into()))?;
    let reg = match cap { runtime::Capability::Scalar(r) => r, _ => return Err(types::Duckerror::Internal("bad capability".into())) };
    let det = types::Funcflags::DETERMINISTIC | types::Funcflags::STATELESS;
    let h = NEXT.fetch_add(1, Ordering::Relaxed); let _ = h;
    reg.register("dice_roll", &[runtime::Funcarg { name: Some("notation".into()), logical: types::Logicaltype::Text }],
        &types::Logicaltype::Int64, runtime::ScalarCallback::new(1),
        Some(&runtime::Funcopts { description: Some("roll dice".into()), tags: vec!["game".into()], attributes: types::Funcflags::empty() }))?;
    reg.register("dice_min", &[runtime::Funcarg { name: Some("notation".into()), logical: types::Logicaltype::Text }],
        &types::Logicaltype::Int64, runtime::ScalarCallback::new(2),
        Some(&runtime::Funcopts { description: Some("minimum roll".into()), tags: vec!["game".into()], attributes: det }))?;
    reg.register("dice_max", &[runtime::Funcarg { name: Some("notation".into()), logical: types::Logicaltype::Text }],
        &types::Logicaltype::Int64, runtime::ScalarCallback::new(3),
        Some(&runtime::Funcopts { description: Some("maximum roll".into()), tags: vec!["game".into()], attributes: det }))?;
    Ok(())
}
use std::sync::atomic::{AtomicU32, Ordering};
static NEXT: AtomicU32 = AtomicU32::new(1);
