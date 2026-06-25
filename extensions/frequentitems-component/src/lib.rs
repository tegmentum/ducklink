//! Top-K frequent-items / heavy-hitters as DuckDB scalars (the DataSketches
//! frequent-items functionality, which DuckDB core lacks as a serializable
//! sketch — core only has `approx_top_k` as an aggregate, not a scalar over a
//! JSON array):
//!   top_k(values_json, k)       -> JSON array of {"value":..,"count":..}
//!   top_k_value(values_json, k) -> JSON array of just the K most frequent values
//!
//! Exact counts over the passed (bounded) array via a hashmap + sort. Ties are
//! broken by first-seen order so the output is fully deterministic. The input is
//! a JSON array; non-string elements are stringified (numbers/bools to their
//! JSON text, null skipped). NULL / bad input / k <= 0 -> NULL. Never panics.
use std::collections::HashMap;
use std::sync::{atomic::{AtomicU32, Ordering}, Mutex, OnceLock};
use wit_bindgen::rt::string::String;
use wit_bindgen::rt::vec::Vec;
wit_bindgen::generate!({ path: "./wit", world: "duckdb:extension/duckdb-extension" });
use duckdb::extension::{runtime, types};
use exports::duckdb::extension::{callback_dispatch, guest};

struct Extension;
impl guest::Guest for Extension {
    fn load() -> Result<types::Loadresult, types::Duckerror> {
        register_scalars()?;
        Ok(types::Loadresult { name: "frequentitems".into(), version: Some(env!("CARGO_PKG_VERSION").into()), requires: Vec::new().into() })
    }
    fn reconfigure(_k: Vec<String>) -> Result<bool, types::Duckerror> { Ok(false) }
    fn shutdown() -> Result<bool, types::Duckerror> { Ok(false) }
}

fn text_arg(args: &[types::Duckvalue], i: usize) -> Option<std::string::String> {
    match args.get(i) { Some(types::Duckvalue::Text(s)) => Some(s.to_string()), _ => None }
}
fn int_arg(args: &[types::Duckvalue], i: usize) -> Option<i64> {
    match args.get(i) {
        Some(types::Duckvalue::Int64(v)) => Some(*v),
        Some(types::Duckvalue::Float64(v)) => Some(*v as i64),
        _ => None,
    }
}

/// Turn a JSON value into the string we count. Strings are used as-is; numbers
/// and bools use their JSON text; nulls are skipped (return None).
fn elem_to_key(v: &serde_json::Value) -> Option<std::string::String> {
    match v {
        serde_json::Value::Null => None,
        serde_json::Value::String(s) => Some(s.clone()),
        serde_json::Value::Bool(b) => Some(b.to_string()),
        serde_json::Value::Number(n) => Some(n.to_string()),
        other => Some(other.to_string()),
    }
}

/// Count occurrences and return the top-K (value, count) pairs. Ordered by
/// count desc, then by first-seen order for deterministic tie-breaks.
fn top_k_pairs(values_json: &str, k: i64) -> Option<std::vec::Vec<(std::string::String, u64)>> {
    if k <= 0 { return None; }
    let parsed: serde_json::Value = serde_json::from_str(values_json).ok()?;
    let arr = parsed.as_array()?;
    let mut counts: HashMap<std::string::String, u64> = HashMap::new();
    let mut order: HashMap<std::string::String, usize> = HashMap::new();
    let mut next_seen: usize = 0;
    for elem in arr {
        if let Some(key) = elem_to_key(elem) {
            *counts.entry(key.clone()).or_insert(0) += 1;
            order.entry(key).or_insert_with(|| { let s = next_seen; next_seen += 1; s });
        }
    }
    let mut pairs: std::vec::Vec<(std::string::String, u64)> = counts.into_iter().collect();
    pairs.sort_by(|a, b| {
        b.1.cmp(&a.1).then_with(|| order[&a.0].cmp(&order[&b.0]))
    });
    pairs.truncate(k as usize);
    Some(pairs)
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
        let json = match text_arg(&args, 0) { Some(s) => s, None => return Ok(types::Duckvalue::Null) };
        let k = match int_arg(&args, 1) { Some(v) => v, None => return Ok(types::Duckvalue::Null) };
        let pairs = match top_k_pairs(&json, k) { Some(p) => p, None => return Ok(types::Duckvalue::Null) };
        let out = match which {
            F::TopK => {
                let arr: std::vec::Vec<serde_json::Value> = pairs.into_iter().map(|(v, c)| {
                    serde_json::json!({ "value": v, "count": c })
                }).collect();
                serde_json::Value::Array(arr).to_string()
            }
            F::TopKValue => {
                let arr: std::vec::Vec<serde_json::Value> = pairs.into_iter()
                    .map(|(v, _)| serde_json::Value::String(v)).collect();
                serde_json::Value::Array(arr).to_string()
            }
        };
        Ok(types::Duckvalue::Text(out.into()))
    }
    fn call_table(_h: u32, _a: Vec<types::Duckvalue>) -> Result<types::Resultset, types::Duckerror> { Err(types::Duckerror::Unsupported("frequentitems: no table fns".into())) }
    fn call_aggregate(_h: u32, _r: types::Rowbatch) -> Result<types::Duckvalue, types::Duckerror> { Err(types::Duckerror::Unsupported("frequentitems: no aggs".into())) }
    fn call_pragma(_h: u32, _a: Vec<types::Duckvalue>) -> Result<Option<types::Duckvalue>, types::Duckerror> { Err(types::Duckerror::Unsupported("frequentitems: no pragmas".into())) }
    fn call_cast(_h: u32, _v: types::Duckvalue) -> Result<types::Duckvalue, types::Duckerror> { Err(types::Duckerror::Unsupported("frequentitems: no casts".into())) }
}
export!(Extension);

fn register_scalars() -> Result<(), types::Duckerror> {
    let cap = runtime::get_capability(types::Capabilitykind::Scalar).ok_or_else(|| types::Duckerror::Internal("no scalar capability".into()))?;
    let reg = match cap { runtime::Capability::Scalar(r) => r, _ => return Err(types::Duckerror::Internal("bad capability".into())) };
    let det = types::Funcflags::DETERMINISTIC | types::Funcflags::STATELESS;
    for (name, f, desc) in [
        ("top_k", F::TopK, "JSON array + K -> JSON array of {value,count} for the K most frequent values"),
        ("top_k_value", F::TopKValue, "JSON array + K -> JSON array of the K most frequent values"),
    ] {
        let h = NEXT.fetch_add(1, Ordering::Relaxed); handlers().lock().unwrap().insert(h, f);
        reg.register(name, &[
            runtime::Funcarg { name: Some("values_json".into()), logical: types::Logicaltype::Text },
            runtime::Funcarg { name: Some("k".into()), logical: types::Logicaltype::Int64 }],
            &types::Logicaltype::Text, runtime::ScalarCallback::new(h),
            Some(&runtime::Funcopts { description: Some(desc.into()), tags: vec!["sketch".into(), "topk".into()], attributes: det }))?;
    }
    Ok(())
}
#[derive(Clone, Copy, PartialEq)] enum F { TopK, TopKValue }
static NEXT: AtomicU32 = AtomicU32::new(1);
static HANDLERS: OnceLock<Mutex<HashMap<u32, F>>> = OnceLock::new();
fn handlers() -> &'static Mutex<HashMap<u32, F>> { HANDLERS.get_or_init(|| Mutex::new(HashMap::new())) }
