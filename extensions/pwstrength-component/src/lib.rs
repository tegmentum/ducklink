//! Password-strength scoring as DuckDB scalars (via the `passwords` crate; pure
//! compute, unlike zxcvbn which needs a wasm-bindgen time import):
//!   password_score(pw) -> double (0-100), password_strength(pw) -> text
//!   (very weak / weak / fair / strong / very strong). NULL -> NULL.
use passwords::{analyzer, scorer};
use wit_bindgen::rt::string::String;
use wit_bindgen::rt::vec::Vec;
wit_bindgen::generate!({ path: "./wit", world: "duckdb:extension/duckdb-extension" });
use duckdb::extension::{runtime, types};
use exports::duckdb::extension::{callback_dispatch, guest};
struct Extension;
impl guest::Guest for Extension {
    fn load() -> Result<types::Loadresult, types::Duckerror> {
        register_scalars()?;
        Ok(types::Loadresult { name: "pwstrength".into(), version: Some(env!("CARGO_PKG_VERSION").into()), requires: Vec::new().into() })
    }
    fn reconfigure(_k: Vec<String>) -> Result<bool, types::Duckerror> { Ok(false) }
    fn shutdown() -> Result<bool, types::Duckerror> { Ok(false) }
}
fn pw(args: &[types::Duckvalue]) -> Option<String> {
    match args.first() { Some(types::Duckvalue::Text(s)) => Some(s.clone()), _ => None }
}
fn label(score: f64) -> &'static str {
    if score < 20.0 { "very weak" } else if score < 40.0 { "weak" }
    else if score < 60.0 { "fair" } else if score < 80.0 { "strong" } else { "very strong" }
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
        let p = match pw(&args) { Some(s) => s, None => return Ok(types::Duckvalue::Null) };
        let score = scorer::score(&analyzer::analyze(&p));
        Ok(match handle {
            1 => types::Duckvalue::Float64(score),
            2 => types::Duckvalue::Text(label(score).into()),
            _ => return Err(types::Duckerror::Internal("unknown scalar handle".into())),
        })
    }
    fn call_table(_h: u32, _a: Vec<types::Duckvalue>) -> Result<types::Resultset, types::Duckerror> { Err(types::Duckerror::Unsupported("pwstrength: no table fns".into())) }
    fn call_aggregate(_h: u32, _r: types::Rowbatch) -> Result<types::Duckvalue, types::Duckerror> { Err(types::Duckerror::Unsupported("pwstrength: no aggs".into())) }
    fn call_pragma(_h: u32, _a: Vec<types::Duckvalue>) -> Result<Option<types::Duckvalue>, types::Duckerror> { Err(types::Duckerror::Unsupported("pwstrength: no pragmas".into())) }
    fn call_cast(_h: u32, _v: types::Duckvalue) -> Result<types::Duckvalue, types::Duckerror> { Err(types::Duckerror::Unsupported("pwstrength: no casts".into())) }
}
export!(Extension);
fn register_scalars() -> Result<(), types::Duckerror> {
    let cap = runtime::get_capability(types::Capabilitykind::Scalar).ok_or_else(|| types::Duckerror::Internal("no scalar capability".into()))?;
    let reg = match cap { runtime::Capability::Scalar(r) => r, _ => return Err(types::Duckerror::Internal("bad capability".into())) };
    let det = types::Funcflags::DETERMINISTIC | types::Funcflags::STATELESS;
    reg.register("password_score", &[runtime::Funcarg { name: Some("password".into()), logical: types::Logicaltype::Text }],
        &types::Logicaltype::Float64, runtime::ScalarCallback::new(1),
        Some(&runtime::Funcopts { description: Some("password score 0-100".into()), tags: vec!["security".into()], attributes: det }))?;
    reg.register("password_strength", &[runtime::Funcarg { name: Some("password".into()), logical: types::Logicaltype::Text }],
        &types::Logicaltype::Text, runtime::ScalarCallback::new(2),
        Some(&runtime::Funcopts { description: Some("password strength label".into()), tags: vec!["security".into()], attributes: det }))?;
    Ok(())
}
