//! CSS color parsing + conversion as DuckDB scalars (via `csscolorparser`):
//!
//!   css_to_hex(text) -> VARCHAR  any CSS color -> '#rrggbb' (or '#rrggbbaa')
//!   css_to_rgb(text) -> VARCHAR  -> 'rgb(r,g,b)' / 'rgba(r,g,b,a)'
//!   css_valid(text)  -> BOOLEAN  true if it parses as a CSS color
//!
//! Accepts named colors, hex, rgb()/rgba(), hsl(), etc. NULL/unparseable ->
//! NULL (css_to_*) or false (css_valid).

use std::collections::HashMap;
use std::sync::{
    atomic::{AtomicU32, Ordering},
    Mutex, OnceLock,
};

use wit_bindgen::rt::string::String;
use wit_bindgen::rt::vec::Vec;

wit_bindgen::generate!({
    path: "./wit",
    world: "duckdb:extension/duckdb-extension",
});

use duckdb::extension::{runtime, types};
use exports::duckdb::extension::{callback_dispatch, guest};

struct Extension;

impl guest::Guest for Extension {
    fn load() -> Result<types::Loadresult, types::Duckerror> {
        register_scalars()?;
        Ok(types::Loadresult {
            name: "csscolor".into(),
            version: Some(env!("CARGO_PKG_VERSION").into()),
            requires: Vec::new().into(),
        })
    }
    fn reconfigure(_keys: Vec<String>) -> Result<bool, types::Duckerror> {
        Ok(false)
    }
    fn shutdown() -> Result<bool, types::Duckerror> {
        Ok(false)
    }
}

fn arg_text(args: &[types::Duckvalue], i: usize, fname: &str) -> Result<Option<String>, types::Duckerror> {
    match args.get(i) {
        Some(types::Duckvalue::Text(s)) => Ok(Some(s.clone())),
        Some(types::Duckvalue::Null) => Ok(None),
        _ => Err(types::Duckerror::Invalidargument(format!(
            "{fname}: expected VARCHAR arg at position {i}"
        ))),
    }
}

impl callback_dispatch::Guest for Extension {
    fn call_scalar_batch(
        handle: u32,
        rows: Vec<Vec<types::Duckvalue>>,
        ctx: types::Invokeinfo,
    ) -> Result<Vec<types::Duckvalue>, types::Duckerror> {
        let base = ctx.rowindex.unwrap_or(0);
        let mut out = Vec::with_capacity(rows.len());
        for (i, args) in rows.into_iter().enumerate() {
            let row_ctx = types::Invokeinfo { rowindex: Some(base + i as u64), iswindow: ctx.iswindow };
            out.push(Self::call_scalar(handle, args, row_ctx)?);
        }
        Ok(out)
    }

    fn call_scalar(
        handle: u32,
        args: Vec<types::Duckvalue>,
        _ctx: types::Invokeinfo,
    ) -> Result<types::Duckvalue, types::Duckerror> {
        let which = scalar_handlers()
            .lock()
            .expect("scalar handler mutex poisoned")
            .get(&handle)
            .copied()
            .ok_or_else(|| types::Duckerror::Internal("unknown scalar handle".into()))?;
        let s = match arg_text(&args, 0, "csscolor")? {
            Some(s) => s,
            None => {
                return Ok(match which {
                    ScalarHandler::Valid => types::Duckvalue::Boolean(false),
                    _ => types::Duckvalue::Null,
                })
            }
        };
        let parsed = csscolorparser::parse(&s);
        Ok(match which {
            ScalarHandler::Valid => types::Duckvalue::Boolean(parsed.is_ok()),
            ScalarHandler::Hex => match parsed {
                Ok(c) => types::Duckvalue::Text(c.to_hex_string()),
                Err(_) => types::Duckvalue::Null,
            },
            ScalarHandler::Rgb => match parsed {
                Ok(c) => types::Duckvalue::Text(c.to_css_rgb()),
                Err(_) => types::Duckvalue::Null,
            },
        })
    }

    fn call_table(_h: u32, _a: Vec<types::Duckvalue>) -> Result<types::Resultset, types::Duckerror> {
        Err(types::Duckerror::Unsupported("csscolor: no table functions".into()))
    }
    fn call_aggregate(_h: u32, _r: types::Rowbatch) -> Result<types::Duckvalue, types::Duckerror> {
        Err(types::Duckerror::Unsupported("csscolor: no aggregates".into()))
    }
    fn call_pragma(_h: u32, _a: Vec<types::Duckvalue>) -> Result<Option<types::Duckvalue>, types::Duckerror> {
        Err(types::Duckerror::Unsupported("csscolor: no pragmas".into()))
    }
    fn call_cast(_h: u32, _v: types::Duckvalue) -> Result<types::Duckvalue, types::Duckerror> {
        Err(types::Duckerror::Unsupported("csscolor: no casts".into()))
    }
}

export!(Extension);

fn register_scalars() -> Result<(), types::Duckerror> {
    let capability = runtime::get_capability(types::Capabilitykind::Scalar)
        .ok_or_else(|| types::Duckerror::Internal("host did not expose scalar capability".into()))?;
    let registry = match capability {
        runtime::Capability::Scalar(registry) => registry,
        _ => return Err(types::Duckerror::Internal("scalar capability returned unexpected variant".into())),
    };
    let det = types::Funcflags::DETERMINISTIC | types::Funcflags::STATELESS;
    register_one(&registry, "css_to_hex", types::Logicaltype::Text, det, ScalarHandler::Hex)?;
    register_one(&registry, "css_to_rgb", types::Logicaltype::Text, det, ScalarHandler::Rgb)?;
    register_one(&registry, "css_valid", types::Logicaltype::Boolean, det, ScalarHandler::Valid)?;
    Ok(())
}

fn register_one(
    registry: &runtime::ScalarRegistry,
    name: &str,
    returns: types::Logicaltype,
    attributes: types::Funcflags,
    handler: ScalarHandler,
) -> Result<(), types::Duckerror> {
    let handle = NEXT_SCALAR_HANDLE.fetch_add(1, Ordering::Relaxed);
    scalar_handlers().lock().expect("scalar handler mutex poisoned").insert(handle, handler);
    let callback = runtime::ScalarCallback::new(handle);
    let args = vec![runtime::Funcarg { name: Some("color".into()), logical: types::Logicaltype::Text }];
    let opts = runtime::Funcopts {
        description: Some("CSS color conversion".into()),
        tags: vec!["csscolor".into()],
        attributes,
    };
    registry.register(name, &args, &returns, callback, Some(&opts))?;
    Ok(())
}

#[derive(Clone, Copy)]
enum ScalarHandler {
    Hex,
    Rgb,
    Valid,
}

static NEXT_SCALAR_HANDLE: AtomicU32 = AtomicU32::new(1);
static SCALAR_HANDLERS: OnceLock<Mutex<HashMap<u32, ScalarHandler>>> = OnceLock::new();

fn scalar_handlers() -> &'static Mutex<HashMap<u32, ScalarHandler>> {
    SCALAR_HANDLERS.get_or_init(|| Mutex::new(HashMap::new()))
}
