//! Amazon Ion <-> JSON conversion as DuckDB scalars (via `ion-rs`):
//!   ion_to_json(data) -> json text  (accepts Ion *text* VARCHAR or binary Ion BLOB),
//!   ion_from_json(json) -> Ion *text*,
//!   ion_get(data, field) -> top-level struct field rendered as text.
//!   NULL input -> NULL; parse error -> NULL; never panics.
use std::collections::HashMap;
use std::sync::{atomic::{AtomicU32, Ordering}, Mutex, OnceLock};
use wit_bindgen::rt::string::String;
use wit_bindgen::rt::vec::Vec;
wit_bindgen::generate!({ path: "./wit", world: "duckdb:extension/duckdb-extension" });
use duckdb::extension::{runtime, types};
use exports::duckdb::extension::{callback_dispatch, guest};

use ion_rs::{Element, IonType, Value};
use serde_json::Value as Json;

struct Extension;
impl guest::Guest for Extension {
    fn load() -> Result<types::Loadresult, types::Duckerror> {
        register_scalars()?;
        Ok(types::Loadresult { name: "ion".into(), version: Some(env!("CARGO_PKG_VERSION").into()), requires: Vec::new().into() })
    }
    fn reconfigure(_k: Vec<String>) -> Result<bool, types::Duckerror> { Ok(false) }
    fn shutdown() -> Result<bool, types::Duckerror> { Ok(false) }
}

// --- argument extraction -----------------------------------------------------

/// Pull the Ion source bytes from arg 0: a Text VARCHAR (Ion text) or a Blob
/// (binary Ion). Returns None for NULL / other types so the caller emits NULL.
fn ion_bytes(args: &[types::Duckvalue], i: usize) -> Option<std::vec::Vec<u8>> {
    match args.get(i) {
        Some(types::Duckvalue::Text(s)) => Some(s.as_bytes().to_vec()),
        Some(types::Duckvalue::Blob(b)) => Some(b.to_vec()),
        _ => None,
    }
}
fn text_arg(args: &[types::Duckvalue], i: usize) -> Option<String> {
    match args.get(i) { Some(types::Duckvalue::Text(s)) => Some(s.clone()), _ => None }
}

// --- Ion element -> JSON -----------------------------------------------------

/// Render an Ion `Element` as a `serde_json::Value`. Ion types with no JSON
/// analogue (timestamps, symbols, decimals, blobs/clobs, s-expressions) fall
/// back to their Ion text rendering as a JSON string so nothing is lost.
fn element_to_json(e: &Element) -> Json {
    if e.is_null() {
        return Json::Null;
    }
    match e.value() {
        Value::Null(_) => Json::Null,
        Value::Bool(b) => Json::Bool(*b),
        Value::Int(_) => match e.as_i64() {
            Some(n) => Json::from(n),
            None => Json::String(e.to_string()),
        },
        Value::Float(f) => serde_json::Number::from_f64(*f)
            .map(Json::Number)
            .unwrap_or(Json::Null),
        Value::Decimal(_) => match e.as_float() {
            Some(f) => serde_json::Number::from_f64(f).map(Json::Number).unwrap_or_else(|| Json::String(e.to_string())),
            None => Json::String(e.to_string()),
        },
        Value::String(s) => Json::String(s.text().to_string()),
        Value::Symbol(s) => match s.text() {
            Some(t) => Json::String(t.to_string()),
            None => Json::Null,
        },
        Value::List(seq) | Value::SExp(seq) => {
            Json::Array(seq.into_iter().map(element_to_json).collect())
        }
        Value::Struct(st) => {
            let mut map = serde_json::Map::new();
            for (name, val) in st {
                let key = name.text().unwrap_or("$0").to_string();
                map.insert(key, element_to_json(val));
            }
            Json::Object(map)
        }
        // Timestamp, Clob, Blob: no faithful JSON form -> Ion text rendering.
        _ => Json::String(e.to_string()),
    }
}

// --- JSON -> Ion element -----------------------------------------------------

/// Build an Ion `Element` from a `serde_json::Value`. Integers stay integers,
/// non-integral numbers become Ion floats, objects become Ion structs.
fn json_to_element(j: &Json) -> Element {
    match j {
        Json::Null => Element::null(IonType::Null),
        Json::Bool(b) => Element::from(*b),
        Json::Number(n) => {
            if let Some(i) = n.as_i64() {
                Element::from(i)
            } else if let Some(u) = n.as_u64() {
                Element::from(u as i64)
            } else {
                Element::from(n.as_f64().unwrap_or(f64::NAN))
            }
        }
        Json::String(s) => Element::from(s.as_str()),
        Json::Array(arr) => {
            let items: std::vec::Vec<Element> = arr.iter().map(json_to_element).collect();
            Element::from(Value::List(ion_rs::Sequence::new(items)))
        }
        Json::Object(map) => {
            let mut b = ion_rs::Struct::builder();
            for (k, v) in map {
                b = b.with_field(k.as_str(), json_to_element(v));
            }
            Element::from(b.build())
        }
    }
}

// --- scalar dispatch ---------------------------------------------------------

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
            F::ToJson => match ion_bytes(&args, 0) {
                None => types::Duckvalue::Null,
                Some(bytes) => match Element::read_one(&bytes) {
                    Ok(el) => match serde_json::to_string(&element_to_json(&el)) {
                        Ok(s) => types::Duckvalue::Text(s.into()),
                        Err(_) => types::Duckvalue::Null,
                    },
                    Err(_) => types::Duckvalue::Null,
                },
            },
            F::FromJson => match text_arg(&args, 0) {
                None => types::Duckvalue::Null,
                Some(s) => match serde_json::from_str::<Json>(&s) {
                    // Element::to_string is infallible; only JSON parse can fail.
                    Ok(j) => types::Duckvalue::Text(json_to_element(&j).to_string().into()),
                    Err(_) => types::Duckvalue::Null,
                },
            },
            F::Get => match (ion_bytes(&args, 0), text_arg(&args, 1)) {
                (Some(bytes), Some(field)) => match Element::read_one(&bytes) {
                    Ok(el) => match el.as_struct().and_then(|st| st.get(field.as_str())) {
                        Some(v) => types::Duckvalue::Text(v.to_string().into()),
                        None => types::Duckvalue::Null,
                    },
                    Err(_) => types::Duckvalue::Null,
                },
                _ => types::Duckvalue::Null,
            },
        })
    }
    fn call_table(_h: u32, _a: Vec<types::Duckvalue>) -> Result<types::Resultset, types::Duckerror> { Err(types::Duckerror::Unsupported("ion: no table fns".into())) }
    fn call_aggregate(_h: u32, _r: types::Rowbatch) -> Result<types::Duckvalue, types::Duckerror> { Err(types::Duckerror::Unsupported("ion: no aggs".into())) }
    fn call_pragma(_h: u32, _a: Vec<types::Duckvalue>) -> Result<Option<types::Duckvalue>, types::Duckerror> { Err(types::Duckerror::Unsupported("ion: no pragmas".into())) }
    fn call_cast(_h: u32, _v: types::Duckvalue) -> Result<types::Duckvalue, types::Duckerror> { Err(types::Duckerror::Unsupported("ion: no casts".into())) }
}
export!(Extension);

fn register_scalars() -> Result<(), types::Duckerror> {
    let cap = runtime::get_capability(types::Capabilitykind::Scalar).ok_or_else(|| types::Duckerror::Internal("no scalar capability".into()))?;
    let reg = match cap { runtime::Capability::Scalar(r) => r, _ => return Err(types::Duckerror::Internal("bad capability".into())) };
    let det = types::Funcflags::DETERMINISTIC | types::Funcflags::STATELESS;

    let h = NEXT.fetch_add(1, Ordering::Relaxed); handlers().lock().unwrap().insert(h, F::ToJson);
    reg.register("ion_to_json",
        &[runtime::Funcarg { name: Some("data".into()), logical: types::Logicaltype::Text }],
        types::Logicaltype::Text, runtime::ScalarCallback::new(h),
        Some(&runtime::Funcopts { description: Some("Ion text/binary -> JSON text".into()), tags: vec!["ion".into(), "json".into()], attributes: det }))?;

    let h = NEXT.fetch_add(1, Ordering::Relaxed); handlers().lock().unwrap().insert(h, F::FromJson);
    reg.register("ion_from_json",
        &[runtime::Funcarg { name: Some("json".into()), logical: types::Logicaltype::Text }],
        types::Logicaltype::Text, runtime::ScalarCallback::new(h),
        Some(&runtime::Funcopts { description: Some("JSON text -> Ion text".into()), tags: vec!["ion".into(), "json".into()], attributes: det }))?;

    let h = NEXT.fetch_add(1, Ordering::Relaxed); handlers().lock().unwrap().insert(h, F::Get);
    reg.register("ion_get",
        &[runtime::Funcarg { name: Some("data".into()), logical: types::Logicaltype::Text },
          runtime::Funcarg { name: Some("field".into()), logical: types::Logicaltype::Text }],
        types::Logicaltype::Text, runtime::ScalarCallback::new(h),
        Some(&runtime::Funcopts { description: Some("top-level Ion struct field as text".into()), tags: vec!["ion".into()], attributes: det }))?;
    Ok(())
}

#[derive(Clone, Copy, PartialEq)] enum F { ToJson, FromJson, Get }
static NEXT: AtomicU32 = AtomicU32::new(1);
static HANDLERS: OnceLock<Mutex<HashMap<u32, F>>> = OnceLock::new();
fn handlers() -> &'static Mutex<HashMap<u32, F>> { HANDLERS.get_or_init(|| Mutex::new(HashMap::new())) }
