//! `ducklink:mosaic` wasm engine — per Phase 0 findings
//! (docs/mosaic-phase0-findings.md) + the Phase 1 build plan in the
//! accepted design memo.
//!
//! Thin, hand-rolled shim (same pattern as `fieldbook-component` /
//! `cache-component`: the standard `duckdb_shim!` macro doesn't hook a
//! load-time bootstrap, and this extension has four non-standard
//! load-time steps beyond scalar registration — install the four
//! bridges [`nested_exec`, `bundle_js`, `index_html`, `token_gen`] into
//! `mosaic-core`, run the `__mosaic_apps` bootstrap, register the
//! `mosaic_list` read macro, and register the six mosaic_* scalars).
//!
//! Everything else — schema, opts parsing, URL layout, vgplot-spec
//! generation, the SQL for the shared `/api/query` route — lives in
//! `mosaic-core` so the whole surface is DB-agnostic.

use std::collections::HashMap;
use std::sync::{
    atomic::{AtomicU32, AtomicU64, Ordering},
    Mutex, OnceLock,
};

use wit_bindgen::rt::string::String as WitString;
use wit_bindgen::rt::vec::Vec as WitVec;

use datalink_extcore::{ExtCore as _, NeutralType, NeutralValue, NullHandling};
use mosaic_core::Core;

wit_bindgen::generate!({
    path: "./wit",
    world: "duckdb:extension/duckdb-extension-mosaic",
});

use duckdb::extension::{catalog, nested_exec as host_nested_exec, runtime, types};
use exports::duckdb::extension::guest;

// ---------------------------------------------------------------------------
// Embedded browser bundle. Rebuild via `make mosaic-browser` (or
// `npm --prefix extensions/mosaic-component/browser run build`).
// The two files are copied straight into the routes table on
// `mosaic_create`; the extension substitutes __NAME__ / __TOKEN__ in
// the index-template.html before writing it.
// ---------------------------------------------------------------------------

static BUNDLE_JS_BYTES: &[u8] = include_bytes!("../browser/dist/bundle.js");
static INDEX_HTML_STR: &str = include_str!("../browser/dist/index.html");

fn bundle_js_provider() -> &'static [u8] {
    BUNDLE_JS_BYTES
}
fn index_html_provider() -> &'static str {
    INDEX_HTML_STR
}

// ---------------------------------------------------------------------------
// Token generator. `getrandom` uses wasi:random on wasip2 (the host's
// wasmtime-wasi wiring satisfies it). 16 bytes -> 32 hex chars.
// ---------------------------------------------------------------------------

fn token_generator() -> std::string::String {
    let mut buf = [0u8; 16];
    if getrandom::getrandom(&mut buf).is_err() {
        // Extremely defensive: mix the wall-clock into a deterministic
        // fallback so we never return an empty token when the host
        // wiring is quirky. Emits 32 hex chars either way.
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos() as u64)
            .unwrap_or(0);
        let nonce = FALLBACK_COUNTER.fetch_add(1, Ordering::Relaxed);
        let mixed = now ^ nonce.wrapping_mul(0x9E37_79B9_7F4A_7C15);
        for (i, b) in mixed.to_le_bytes().iter().enumerate() {
            buf[i] = *b;
        }
        for i in 8..16 {
            buf[i] = buf[i - 8] ^ 0xA5;
        }
    }
    let mut out = std::string::String::with_capacity(32);
    for b in buf.iter() {
        out.push(HEX[(b >> 4) as usize] as char);
        out.push(HEX[(b & 0x0f) as usize] as char);
    }
    out
}
static FALLBACK_COUNTER: AtomicU64 = AtomicU64::new(0);
const HEX: &[u8] = b"0123456789abcdef";

// ---------------------------------------------------------------------------
// Handle table (u32 -> DECLS index). Same layout the `duckdb_shim!`
// macro uses.
// ---------------------------------------------------------------------------

fn handles() -> &'static Mutex<HashMap<u32, usize>> {
    static T: OnceLock<Mutex<HashMap<u32, usize>>> = OnceLock::new();
    T.get_or_init(|| Mutex::new(HashMap::new()))
}
static NEXT_HANDLE: AtomicU32 = AtomicU32::new(1);

// ---------------------------------------------------------------------------
// Bridge: shim -> mosaic-core nested-exec fn pointer.
// ---------------------------------------------------------------------------

fn shim_nested_exec(sql: &str) -> Result<mosaic_core::ExecResult, std::string::String> {
    match host_nested_exec::nested_exec(sql) {
        Ok(r) => Ok(mosaic_core::ExecResult {
            rows: r.rows.map(|rs| {
                rs.into_iter()
                    .map(|row| row.into_iter().map(std::string::String::from).collect())
                    .collect()
            }),
            rows_affected: r.rows_affected,
        }),
        Err(e) => Err(std::string::String::from(e.as_str())),
    }
}

// ---------------------------------------------------------------------------
// Marshalling: Duckvalue <-> NeutralValue. FROZEN set + `complex(...)`
// escape hatch, kept in sync with datalink-extcore/src/shim_duckdb.rs.
// ---------------------------------------------------------------------------

fn to_neutral(v: &types::Duckvalue) -> NeutralValue {
    match v {
        types::Duckvalue::Null => NeutralValue::Null,
        types::Duckvalue::Boolean(b) => NeutralValue::Boolean(*b),
        types::Duckvalue::Int64(n) => NeutralValue::Int64(*n),
        types::Duckvalue::Float64(f) => NeutralValue::Float64(*f),
        types::Duckvalue::Text(s) => NeutralValue::Text(std::string::String::from(s.as_str())),
        types::Duckvalue::Blob(b) => NeutralValue::Blob(b.clone()),
        types::Duckvalue::Complex(c) => NeutralValue::Complex {
            type_expr: std::string::String::from(c.type_expr.as_str()),
            json: std::string::String::from(c.json.as_str()),
        },
        other => NeutralValue::Complex {
            type_expr: std::string::String::from("UNSUPPORTED"),
            json: std::format!("{:?}", other),
        },
    }
}

fn from_neutral(v: NeutralValue) -> types::Duckvalue {
    match v {
        NeutralValue::Null => types::Duckvalue::Null,
        NeutralValue::Boolean(b) => types::Duckvalue::Boolean(b),
        NeutralValue::Int64(n) => types::Duckvalue::Int64(n),
        NeutralValue::Float64(f) => types::Duckvalue::Float64(f),
        NeutralValue::Text(s) => types::Duckvalue::Text(s.into()),
        NeutralValue::Blob(b) => types::Duckvalue::Blob(b),
        NeutralValue::Complex { type_expr, json } => {
            types::Duckvalue::Complex(types::Complexvalue {
                type_expr: type_expr.into(),
                json: json.into(),
            })
        }
    }
}

fn ntype_to_logical(t: &NeutralType) -> types::Logicaltype {
    match t {
        NeutralType::Boolean => types::Logicaltype::Boolean,
        NeutralType::Int64 => types::Logicaltype::Int64,
        NeutralType::Float64 => types::Logicaltype::Float64,
        NeutralType::Text => types::Logicaltype::Text,
        NeutralType::Blob => types::Logicaltype::Blob,
        NeutralType::Complex(e) => types::Logicaltype::Complex(e.clone().into()),
    }
}

fn duckerr(e: std::string::String) -> types::Duckerror {
    types::Duckerror::Invalidargument(e)
}

// ---------------------------------------------------------------------------
// Registration.
// ---------------------------------------------------------------------------

fn register_scalars() -> Result<(), types::Duckerror> {
    let cap = runtime::get_capability(types::Capabilitykind::Scalar)
        .ok_or_else(|| types::Duckerror::Internal("no scalar capability".into()))?;
    let reg = match cap {
        runtime::Capability::Scalar(r) => r,
        _ => return Err(types::Duckerror::Internal("bad scalar capability".into())),
    };
    for (idx, decl) in Core::DECLS.iter().enumerate() {
        let handle = NEXT_HANDLE.fetch_add(1, Ordering::Relaxed);
        handles().lock().expect("poisoned").insert(handle, idx);
        let args: WitVec<runtime::Funcarg> = decl
            .args
            .iter()
            .map(|t| runtime::Funcarg {
                name: Some("value".into()),
                logical: ntype_to_logical(t),
            })
            .collect();
        let mut attributes = types::Funcflags::STATELESS;
        if decl.deterministic {
            attributes |= types::Funcflags::DETERMINISTIC;
        }
        // Rewrite the SQL name so the two mosaic_create overloads (arity
        // 2 + 3) both register as `mosaic_create` — DuckDB dispatches by
        // arity. All other decls pass through with their declared
        // identifier verbatim.
        let sql_name = mosaic_core::registered_name_for(decl.name);
        let opts = runtime::Funcopts {
            description: Some(std::format!(
                "mosaic scalar {} (arity {})",
                sql_name,
                decl.args.len()
            )),
            tags: std::vec!["mosaic".into()],
            attributes,
        };
        reg.register(
            sql_name,
            &args,
            &ntype_to_logical(&decl.ret),
            runtime::ScalarCallback::new(handle),
            Some(&opts),
        )?;
    }
    Ok(())
}

fn register_macros() -> Result<(), types::Duckerror> {
    for m in mosaic_core::READ_MACROS {
        let params: WitVec<WitString> = m.parameters.iter().map(|s| (*s).into()).collect();
        let def = catalog::MacroDef {
            schema: WitString::new(),
            name: m.name.into(),
            parameters: params,
            definition_sql: m.body_sql.into(),
        };
        catalog::register_macro(&def).map_err(types::Duckerror::Internal)?;
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// guest::Guest.
// ---------------------------------------------------------------------------

struct Extension;

impl guest::Guest for Extension {
    fn load() -> Result<types::Loadresult, types::Duckerror> {
        // 1) Wire the four bridges into mosaic-core.
        mosaic_core::install_nested_exec(shim_nested_exec);
        mosaic_core::install_bundle_js(bundle_js_provider);
        mosaic_core::install_index_html(index_html_provider);
        mosaic_core::install_token_gen(token_generator);

        // 2) Bootstrap the __mosaic_apps table. Non-fatal if the host
        //    hasn't wired nested-exec yet: the table gets created lazily
        //    on the first mosaic_create call.
        let _ = mosaic_core::nested_exec(&mosaic_core::bootstrap_sql());

        // 3) Register mosaic_list.
        register_macros()?;

        // 4) Register the six scalars (mosaic_create x2, drop, url,
        //    spec, plot).
        register_scalars()?;

        Ok(types::Loadresult {
            name: <Core as datalink_extcore::ExtCore>::NAME.into(),
            version: Some(<Core as datalink_extcore::ExtCore>::VERSION.into()),
            requires: WitVec::new().into(),
        })
    }

    fn reconfigure(_keys: WitVec<WitString>) -> Result<bool, types::Duckerror> {
        Ok(false)
    }

    fn shutdown() -> Result<bool, types::Duckerror> {
        Ok(false)
    }
}

// ---------------------------------------------------------------------------
// Per-row scalar entry point (fanned out by the columnar bridge below).
// ---------------------------------------------------------------------------

fn mosaic_scalar(
    handle: u32,
    args: WitVec<types::Duckvalue>,
    _ctx: types::Invokeinfo,
) -> Result<types::Duckvalue, types::Duckerror> {
    let idx = handles()
        .lock()
        .expect("poisoned")
        .get(&handle)
        .copied()
        .ok_or_else(|| types::Duckerror::Internal("unknown scalar handle".into()))?;
    let decl = &Core::DECLS[idx];
    let neutral: std::vec::Vec<NeutralValue> = args.iter().map(to_neutral).collect();
    if matches!(decl.null_handling, NullHandling::Propagate)
        && neutral.iter().any(|v| v.is_null())
    {
        return Ok(types::Duckvalue::Null);
    }
    let res = Core::dispatch(idx, &neutral).map_err(duckerr)?;
    Ok(from_neutral(res))
}

datalink_extcore::columnar_bridge! {
    types = duckdb::extension::types;
    column_types = duckdb::extension::column_types;
    callback_dispatch = exports::duckdb::extension::callback_dispatch;
    target = Extension;
    scalar = mosaic_scalar;
}

export!(Extension);
