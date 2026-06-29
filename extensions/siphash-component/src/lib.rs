//! Keyed SipHash-1-3 as a DuckDB scalar (via `siphasher`):
//!   siphash(key0, key1, text) -> ubigint. Fast keyed hash for hash-flooding-
//!   resistant bucketing. NULL text -> NULL.
use std::hash::Hasher;
use siphasher::sip::SipHasher13;
use wit_bindgen::rt::string::String;
use wit_bindgen::rt::vec::Vec;
wit_bindgen::generate!({ path: "./wit", world: "duckdb:extension/duckdb-extension" });
use duckdb::extension::{runtime, types};
use exports::duckdb::extension::guest;
struct Extension;
impl guest::Guest for Extension {
    fn load() -> Result<types::Loadresult, types::Duckerror> {
        register_scalars()?;
        Ok(types::Loadresult { name: "siphash".into(), version: Some(env!("CARGO_PKG_VERSION").into()), requires: Vec::new().into() })
    }
    fn reconfigure(_k: Vec<String>) -> Result<bool, types::Duckerror> { Ok(false) }
    fn shutdown() -> Result<bool, types::Duckerror> { Ok(false) }
}
fn key(args: &[types::Duckvalue], i: usize) -> u64 {
    match args.get(i) { Some(types::Duckvalue::Int64(n)) => *n as u64, Some(types::Duckvalue::Uint64(n)) => *n, _ => 0 }
}
// Per-row scalar logic, UNCHANGED. NOTE: siphash returns UBIGINT
// (Duckvalue::Uint64), which the neutral pull-up type set cannot express, so this
// stays a hand-written component bridged to the major-4 columnar dispatch via
// datalink_extcore::columnar_bridge!.
fn scalar(_handle: u32, args: Vec<types::Duckvalue>, _c: types::Invokeinfo) -> Result<types::Duckvalue, types::Duckerror> {
    let text = match args.get(2) { Some(types::Duckvalue::Text(s)) => s.clone(), _ => return Ok(types::Duckvalue::Null) };
    let mut hasher = SipHasher13::new_with_keys(key(&args, 0), key(&args, 1));
    hasher.write(text.as_bytes());
    Ok(types::Duckvalue::Uint64(hasher.finish()))
}

// TRUE column-at-a-time kernel for the columnar (@4.0.0) hot path. The generic
// `columnar_bridge!` would lift the three input columns into a
// `Vec<Vec<Duckvalue>>` rowbatch (cloning every input string into a
// `Duckvalue::Text`) before calling the per-row `scalar`. siphash is
// guest-compute-dominated, but that row-materialization is pure overhead on top
// of the hashing. This kernel borrows each input string in place (no clone, no
// boxing), reads the two key columns directly, and writes the UBIGINT output
// column directly. Byte-identical to `scalar`: same SipHash-1-3 keys + bytes +
// NULL handling (NULL text -> NULL; a NULL key reads as 0, matching `key()`).
use duckdb::extension::column_types as col;
fn key_col_at(c: Option<&col::Colvec>, i: usize) -> u64 {
    match c {
        Some(c) => {
            let v = c.validity.is_empty()
                || c.validity.get(i / 8).map(|b| (b >> (i % 8)) & 1 == 1).unwrap_or(false);
            if !v { return 0; }
            match &c.data {
                col::Column::Int64(d) => d[i] as u64,
                col::Column::Uint64(d) => d[i],
                _ => 0,
            }
        }
        None => 0,
    }
}
fn scalar_batch_col(
    _handle: u32,
    args: &[col::Colvec],
    _ctx: types::Invokeinfo,
) -> Result<col::Colvec, types::Duckerror> {
    let k0 = args.first();
    let k1 = args.get(1);
    let textcol = args.get(2)
        .ok_or_else(|| types::Duckerror::Internal("siphash: missing text argument".into()))?;
    let n = textcol.rows as usize;
    let valid = |i: usize| -> bool {
        textcol.validity.is_empty()
            || textcol.validity.get(i / 8).map(|b| (b >> (i % 8)) & 1 == 1).unwrap_or(false)
    };
    let texts: Option<&Vec<String>> = match &textcol.data { col::Column::Text(v) => Some(v), _ => None };
    let mut out = Vec::with_capacity(n);
    let mut validity = vec![0u8; n.div_ceil(8)];
    let mut any_null = false;
    for i in 0..n {
        match texts {
            Some(t) if valid(i) => {
                let mut hasher = SipHasher13::new_with_keys(key_col_at(k0, i), key_col_at(k1, i));
                hasher.write(t[i].as_bytes());
                out.push(hasher.finish());
                validity[i / 8] |= 1 << (i % 8);
            }
            _ => { out.push(0u64); any_null = true; }
        }
    }
    let validity = if any_null { validity } else { Vec::new() };
    Ok(col::Colvec { data: col::Column::Uint64(out), validity, rows: textcol.rows })
}

datalink_extcore::columnar_bridge! {
    types = duckdb::extension::types;
    column_types = duckdb::extension::column_types;
    callback_dispatch = exports::duckdb::extension::callback_dispatch;
    target = Extension;
    scalar = scalar;
    scalar_batch_col = scalar_batch_col;
}
export!(Extension);
fn register_scalars() -> Result<(), types::Duckerror> {
    let cap = runtime::get_capability(types::Capabilitykind::Scalar).ok_or_else(|| types::Duckerror::Internal("no scalar capability".into()))?;
    let reg = match cap { runtime::Capability::Scalar(r) => r, _ => return Err(types::Duckerror::Internal("bad capability".into())) };
    let det = types::Funcflags::DETERMINISTIC | types::Funcflags::STATELESS;
    reg.register("siphash", &[
        runtime::Funcarg { name: Some("key0".into()), logical: types::Logicaltype::Int64 },
        runtime::Funcarg { name: Some("key1".into()), logical: types::Logicaltype::Int64 },
        runtime::Funcarg { name: Some("text".into()), logical: types::Logicaltype::Text }],
        &types::Logicaltype::Uint64, runtime::ScalarCallback::new(1),
        Some(&runtime::Funcopts { description: Some("keyed SipHash-1-3".into()), tags: vec!["hash".into()], attributes: det }))?;
    Ok(())
}
