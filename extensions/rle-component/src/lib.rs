//! Run-length encoding as DuckDB scalars (hand-rolled): rle_encode(text) ->
//! "<count><char>..." (e.g. "aaabbc" -> "3a2b1c"), rle_decode(text) inverts it.
//! Decode of malformed input -> NULL. NULL -> NULL.
use wit_bindgen::rt::string::String;
use wit_bindgen::rt::vec::Vec;
wit_bindgen::generate!({ path: "./wit", world: "duckdb:extension/duckdb-extension" });
use duckdb::extension::{runtime, types};
use exports::duckdb::extension::{callback_dispatch, guest};
struct Extension;
impl guest::Guest for Extension {
    fn load() -> Result<types::Loadresult, types::Duckerror> {
        register_scalars()?;
        Ok(types::Loadresult { name: "rle".into(), version: Some(env!("CARGO_PKG_VERSION").into()), requires: Vec::new().into() })
    }
    fn reconfigure(_k: Vec<String>) -> Result<bool, types::Duckerror> { Ok(false) }
    fn shutdown() -> Result<bool, types::Duckerror> { Ok(false) }
}
fn encode(s: &str) -> std::string::String {
    let mut out = std::string::String::new();
    let chars: std::vec::Vec<char> = s.chars().collect();
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i]; let mut n = 1;
        while i + n < chars.len() && chars[i + n] == c { n += 1; }
        out.push_str(&n.to_string()); out.push(c); i += n;
    }
    out
}
fn decode(s: &str) -> Option<std::string::String> {
    let mut out = std::string::String::new();
    let mut num = std::string::String::new();
    for c in s.chars() {
        if c.is_ascii_digit() { num.push(c); }
        else {
            if num.is_empty() { return None; }
            let n: usize = num.parse().ok()?;
            for _ in 0..n { out.push(c); }
            num.clear();
        }
    }
    if num.is_empty() { Some(out) } else { None }
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
        let s = match args.first() { Some(types::Duckvalue::Text(s)) => s.clone(), _ => return Ok(types::Duckvalue::Null) };
        Ok(if handle == 1 {
            types::Duckvalue::Text(encode(&s).into())
        } else {
            match decode(&s) { Some(t) => types::Duckvalue::Text(t.into()), None => types::Duckvalue::Null }
        })
    }
    fn call_table(_h: u32, _a: Vec<types::Duckvalue>) -> Result<types::Resultset, types::Duckerror> { Err(types::Duckerror::Unsupported("rle: no table fns".into())) }
    fn call_aggregate(_h: u32, _r: types::Rowbatch) -> Result<types::Duckvalue, types::Duckerror> { Err(types::Duckerror::Unsupported("rle: no aggs".into())) }
    fn call_pragma(_h: u32, _a: Vec<types::Duckvalue>) -> Result<Option<types::Duckvalue>, types::Duckerror> { Err(types::Duckerror::Unsupported("rle: no pragmas".into())) }
    fn call_cast(_h: u32, _v: types::Duckvalue) -> Result<types::Duckvalue, types::Duckerror> { Err(types::Duckerror::Unsupported("rle: no casts".into())) }
}
export!(Extension);
fn register_scalars() -> Result<(), types::Duckerror> {
    let cap = runtime::get_capability(types::Capabilitykind::Scalar).ok_or_else(|| types::Duckerror::Internal("no scalar capability".into()))?;
    let reg = match cap { runtime::Capability::Scalar(r) => r, _ => return Err(types::Duckerror::Internal("bad capability".into())) };
    let det = types::Funcflags::DETERMINISTIC | types::Funcflags::STATELESS;
    reg.register("rle_encode", &[runtime::Funcarg { name: Some("text".into()), logical: types::Logicaltype::Text }],
        &types::Logicaltype::Text, runtime::ScalarCallback::new(1),
        Some(&runtime::Funcopts { description: Some("run-length encode".into()), tags: vec!["encoding".into()], attributes: det }))?;
    reg.register("rle_decode", &[runtime::Funcarg { name: Some("text".into()), logical: types::Logicaltype::Text }],
        &types::Logicaltype::Text, runtime::ScalarCallback::new(2),
        Some(&runtime::Funcopts { description: Some("run-length decode".into()), tags: vec!["encoding".into()], attributes: det }))?;
    Ok(())
}
