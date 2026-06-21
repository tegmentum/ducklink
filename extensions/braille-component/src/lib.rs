//! Text -> Unicode Braille as a DuckDB scalar (hand-rolled, grade-1 letters):
//!   to_braille(text) maps a-z to the Unicode Braille Patterns block; spaces and
//!   unmapped characters pass through. NULL -> NULL.
use wit_bindgen::rt::string::String;
use wit_bindgen::rt::vec::Vec;
wit_bindgen::generate!({ path: "./wit", world: "duckdb:extension/duckdb-extension" });
use duckdb::extension::{runtime, types};
use exports::duckdb::extension::{callback_dispatch, guest};
struct Extension;
// a-z in standard English Braille (Unicode Braille Patterns U+2800 block).
const LETTERS: [char; 26] = ['\u{2801}','\u{2803}','\u{2809}','\u{2819}','\u{2811}','\u{280B}','\u{281B}',
    '\u{2813}','\u{280A}','\u{281A}','\u{2805}','\u{2807}','\u{280D}','\u{281D}','\u{2815}','\u{280F}',
    '\u{281F}','\u{2817}','\u{280E}','\u{281E}','\u{2825}','\u{2827}','\u{283A}','\u{282D}','\u{283D}','\u{2835}'];
fn to_braille(s: &str) -> std::string::String {
    s.chars().map(|c| {
        let lc = c.to_ascii_lowercase();
        if lc.is_ascii_lowercase() { LETTERS[(lc as u8 - b'a') as usize] } else { c }
    }).collect()
}
impl guest::Guest for Extension {
    fn load() -> Result<types::Loadresult, types::Duckerror> {
        register_scalars()?;
        Ok(types::Loadresult { name: "braille".into(), version: Some(env!("CARGO_PKG_VERSION").into()), requires: Vec::new().into() })
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
    fn call_scalar(_handle: u32, args: Vec<types::Duckvalue>, _c: types::Invokeinfo) -> Result<types::Duckvalue, types::Duckerror> {
        match args.first() {
            Some(types::Duckvalue::Text(s)) => Ok(types::Duckvalue::Text(to_braille(s).into())),
            _ => Ok(types::Duckvalue::Null),
        }
    }
    fn call_table(_h: u32, _a: Vec<types::Duckvalue>) -> Result<types::Resultset, types::Duckerror> { Err(types::Duckerror::Unsupported("braille: no table fns".into())) }
    fn call_aggregate(_h: u32, _r: types::Rowbatch) -> Result<types::Duckvalue, types::Duckerror> { Err(types::Duckerror::Unsupported("braille: no aggs".into())) }
    fn call_pragma(_h: u32, _a: Vec<types::Duckvalue>) -> Result<Option<types::Duckvalue>, types::Duckerror> { Err(types::Duckerror::Unsupported("braille: no pragmas".into())) }
    fn call_cast(_h: u32, _v: types::Duckvalue) -> Result<types::Duckvalue, types::Duckerror> { Err(types::Duckerror::Unsupported("braille: no casts".into())) }
}
export!(Extension);
fn register_scalars() -> Result<(), types::Duckerror> {
    let cap = runtime::get_capability(types::Capabilitykind::Scalar).ok_or_else(|| types::Duckerror::Internal("no scalar capability".into()))?;
    let reg = match cap { runtime::Capability::Scalar(r) => r, _ => return Err(types::Duckerror::Internal("bad capability".into())) };
    let det = types::Funcflags::DETERMINISTIC | types::Funcflags::STATELESS;
    reg.register("to_braille", &[runtime::Funcarg { name: Some("text".into()), logical: types::Logicaltype::Text }],
        types::Logicaltype::Text, runtime::ScalarCallback::new(1),
        Some(&runtime::Funcopts { description: Some("text -> Braille".into()), tags: vec!["accessibility".into()], attributes: det }))?;
    Ok(())
}
