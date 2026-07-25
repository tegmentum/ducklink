//! `ducklink:fieldbook` wasm engine — the wasm-side counterpart to the
//! native `duckdb-fieldbook` extension.
//!
//! Thin, hand-rolled shim (the standard `duckdb_shim!` macro from
//! `datalink-extcore` doesn't yet accept a `load()` hook, and this
//! extension has three non-standard load-time steps beyond scalar
//! registration: install the `nested-exec` function pointer into
//! `fieldbook-core`, run the CREATE TABLE bootstrap, and register the
//! three read macros via the `runtime.macro-registry`). All function
//! logic + the four-scalar capability table live ONCE in
//! `fieldbook-core` (datalink); this file iterates
//! `<Core as ExtCore>::DECLS` to derive the registration.
//!
//! COORDINATION NOTE (task #7): the `nested-exec` host import is
//! defined at `wit/duckdb-extension/nested-exec.wit`; the host-side
//! implementation in `crates/ducklink-host` is being wired up in
//! parallel. Until that lands, the CREATE TABLE bootstrap and every
//! scalar invocation will trap on the missing import — this shim
//! compiles and produces a valid component artifact regardless.

use std::collections::HashMap;
use std::sync::{
    atomic::{AtomicU32, Ordering},
    Mutex, OnceLock,
};

use wit_bindgen::rt::string::String as WitString;
use wit_bindgen::rt::vec::Vec as WitVec;

use datalink_extcore::{ExtCore as _, NeutralType, NeutralValue, NullHandling};
use fieldbook_core::Core;

wit_bindgen::generate!({
    path: "./wit",
    world: "duckdb:extension/duckdb-extension-fieldbook",
});

use duckdb::extension::{catalog, nested_exec as host_nested_exec, runtime, types};
use exports::duckdb::extension::guest;

// ---------------------------------------------------------------------------
// Handle table (u32 -> DECLS index). Mirrors the layout `duckdb_shim!` uses.
// ---------------------------------------------------------------------------

fn handles() -> &'static Mutex<HashMap<u32, usize>> {
    static T: OnceLock<Mutex<HashMap<u32, usize>>> = OnceLock::new();
    T.get_or_init(|| Mutex::new(HashMap::new()))
}
static NEXT_HANDLE: AtomicU32 = AtomicU32::new(1);

// ---------------------------------------------------------------------------
// Bridge: shim -> fieldbook-core nested-exec fn pointer.
//
// wit-bindgen generates `host_nested_exec::nested_exec(sql: &str) -> Result<
// host_nested_exec::ExecResult, WitString>`. Wrap that in a fn pointer whose
// signature matches `fieldbook_core::NestedExecFn`.
// ---------------------------------------------------------------------------

fn shim_nested_exec(sql: &str) -> Result<fieldbook_core::ExecResult, std::string::String> {
    match host_nested_exec::nested_exec(sql) {
        Ok(r) => Ok(fieldbook_core::ExecResult {
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
// Marshalling: Duckvalue <-> NeutralValue.
//
// Same closed FROZEN set + `complex(...)` escape hatch as the
// `duckdb_shim!` macro emits (kept in sync with
// datalink-extcore/src/shim_duckdb.rs::to_neutral/from_neutral).
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
        // The host casts to the registered logicaltype before calling us,
        // and every fieldbook argument is Text or Int64. Route anything
        // else through the escape hatch (matches `duckdb_shim!`).
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
// Registration
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
        let opts = runtime::Funcopts {
            description: Some(std::format!("fieldbook scalar {}", decl.name)),
            tags: std::vec!["fieldbook".into()],
            attributes,
        };
        reg.register(
            decl.name,
            &args,
            &ntype_to_logical(&decl.ret),
            runtime::ScalarCallback::new(handle),
            Some(&opts),
        )?;
    }
    Ok(())
}

fn register_macros() -> Result<(), types::Duckerror> {
    // The runtime.macro-registry capability path is intentionally Unsupported
    // in ducklink (see crates/ducklink-runtime `Capabilitykind::Macro`); macros
    // register through the sibling `catalog.register-macro` interface, which
    // the ducklink native extension drains into `CREATE OR REPLACE MACRO`
    // statements after component load. Schema is left empty (defaults to
    // `main`); parameters + body come straight from fieldbook-core's
    // `READ_MACROS` table.
    for m in fieldbook_core::READ_MACROS {
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
// guest::Guest (load / reconfigure / shutdown)
// ---------------------------------------------------------------------------

struct Extension;

impl guest::Guest for Extension {
    fn load() -> Result<types::Loadresult, types::Duckerror> {
        // 1) Wire the nested-exec bridge into fieldbook-core so scalar
        //    bodies can run SQL via `fieldbook_core::nested_exec`.
        fieldbook_core::install_nested_exec(shim_nested_exec);

        // 2) Bootstrap the three storage tables. Non-fatal if the host
        //    hasn't wired the nested-exec import yet (task #7): the
        //    tables can be created lazily on first mutate call. A hard
        //    fail here would prevent the component from loading at all.
        let _ = fieldbook_core::nested_exec(&fieldbook_core::bootstrap_sql());

        // 3) Register the three read-side SQL macros.
        register_macros()?;

        // 4) Register the four mutate/record scalars.
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
// callback_dispatch::Guest — generated via `columnar_bridge!` so we get a
// proper `call_scalar_batch_col` that pivots host-side colvecs through the
// per-row `call_scalar` below. `columnar_stub!()` would only stub those
// methods and the ducklink runtime calls the columnar hot path directly with
// no row-major fallback -- so a stubbed columnar impl surfaces as
// `no scalar functions (bridge stub)` at the first fieldbook_* call.
// ---------------------------------------------------------------------------

/// Per-row scalar entry point the columnar bridge fans out to. Free fn (not a
/// method) so `columnar_bridge!` can name it as a path.
fn fieldbook_scalar(
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
    scalar = fieldbook_scalar;
}

export!(Extension);
