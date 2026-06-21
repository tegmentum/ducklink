//! EAN / UPC barcode validation + check-digit computation as DuckDB scalars.
//!
//!   ean_validate(text)    -> BOOLEAN  valid EAN-8, UPC-A (12), or EAN-13
//!   ean_check_digit(text) -> BIGINT   the check digit for the given body digits
//!
//! Whitespace and hyphens are ignored. The check digit weights the body digits
//! 3,1,3,1,… from the digit adjacent to the check position; this is uniform
//! across EAN-8/UPC-A/EAN-13.

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
            name: "ean".into(),
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

// ---- Algorithm (DB-agnostic) ----

fn digits(s: &str) -> Option<std::vec::Vec<u32>> {
    let mut out = std::vec::Vec::with_capacity(s.len());
    for c in s.chars() {
        if c.is_whitespace() || c == '-' {
            continue;
        }
        out.push(c.to_digit(10)?);
    }
    if out.is_empty() {
        None
    } else {
        Some(out)
    }
}

/// Check digit for `body` (the data digits, WITHOUT the trailing check digit).
fn check_digit(body: &[u32]) -> u32 {
    let mut sum = 0u32;
    for (i, &d) in body.iter().rev().enumerate() {
        sum += if i % 2 == 0 { d * 3 } else { d };
    }
    (10 - (sum % 10)) % 10
}

fn validate(num: &[u32]) -> bool {
    matches!(num.len(), 8 | 12 | 13) && check_digit(&num[..num.len() - 1]) == num[num.len() - 1]
}

// ---- Arg helper ----

fn arg_text(args: &[types::Duckvalue], i: usize, fname: &str) -> Result<String, types::Duckerror> {
    match args.get(i) {
        Some(types::Duckvalue::Text(s)) => Ok(s.clone()),
        Some(types::Duckvalue::Null) => Ok(String::new()),
        _ => Err(types::Duckerror::Invalidargument(format!(
            "{fname}: expected VARCHAR arg at position {i}"
        ))),
    }
}

// ---- Dispatch ----

impl callback_dispatch::Guest for Extension {
    fn call_scalar_batch(
        handle: u32,
        rows: Vec<Vec<types::Duckvalue>>,
        ctx: types::Invokeinfo,
    ) -> Result<Vec<types::Duckvalue>, types::Duckerror> {
        let base = ctx.rowindex.unwrap_or(0);
        let mut out = Vec::with_capacity(rows.len());
        for (i, args) in rows.into_iter().enumerate() {
            let row_ctx = types::Invokeinfo {
                rowindex: Some(base + i as u64),
                iswindow: ctx.iswindow,
            };
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
        let raw = arg_text(&args, 0, "ean")?;
        Ok(match which {
            ScalarHandler::Validate => {
                types::Duckvalue::Boolean(digits(&raw).map(|d| validate(&d)).unwrap_or(false))
            }
            ScalarHandler::CheckDigit => match digits(&raw) {
                Some(d) => types::Duckvalue::Int64(check_digit(&d) as i64),
                None => types::Duckvalue::Null,
            },
        })
    }

    fn call_table(
        _handle: u32,
        _args: Vec<types::Duckvalue>,
    ) -> Result<types::Resultset, types::Duckerror> {
        Err(types::Duckerror::Unsupported("ean: no table functions".into()))
    }
    fn call_aggregate(
        _handle: u32,
        _rows: types::Rowbatch,
    ) -> Result<types::Duckvalue, types::Duckerror> {
        Err(types::Duckerror::Unsupported("ean: no aggregates".into()))
    }
    fn call_pragma(
        _handle: u32,
        _args: Vec<types::Duckvalue>,
    ) -> Result<Option<types::Duckvalue>, types::Duckerror> {
        Err(types::Duckerror::Unsupported("ean: no pragmas".into()))
    }
    fn call_cast(
        _handle: u32,
        _value: types::Duckvalue,
    ) -> Result<types::Duckvalue, types::Duckerror> {
        Err(types::Duckerror::Unsupported("ean: no casts".into()))
    }
}

export!(Extension);

// ---- Registration ----

fn register_scalars() -> Result<(), types::Duckerror> {
    let capability = runtime::get_capability(types::Capabilitykind::Scalar)
        .ok_or_else(|| types::Duckerror::Internal("host did not expose scalar capability".into()))?;
    let registry = match capability {
        runtime::Capability::Scalar(registry) => registry,
        _ => return Err(types::Duckerror::Internal("scalar capability returned unexpected variant".into())),
    };
    let det = types::Funcflags::DETERMINISTIC | types::Funcflags::STATELESS;
    register_one(&registry, "ean_validate", types::Logicaltype::Boolean, det, ScalarHandler::Validate)?;
    register_one(&registry, "ean_check_digit", types::Logicaltype::Int64, det, ScalarHandler::CheckDigit)?;
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
    scalar_handlers()
        .lock()
        .expect("scalar handler mutex poisoned")
        .insert(handle, handler);
    let callback = runtime::ScalarCallback::new(handle);
    let args = vec![runtime::Funcarg {
        name: Some("value".into()),
        logical: types::Logicaltype::Text,
    }];
    let opts = runtime::Funcopts {
        description: Some("EAN/UPC barcode helper".into()),
        tags: vec!["ean".into()],
        attributes,
    };
    registry.register(name, &args, returns, callback, Some(&opts))?;
    Ok(())
}

#[derive(Clone, Copy)]
enum ScalarHandler {
    Validate,
    CheckDigit,
}

static NEXT_SCALAR_HANDLE: AtomicU32 = AtomicU32::new(1);
static SCALAR_HANDLERS: OnceLock<Mutex<HashMap<u32, ScalarHandler>>> = OnceLock::new();

fn scalar_handlers() -> &'static Mutex<HashMap<u32, ScalarHandler>> {
    SCALAR_HANDLERS.get_or_init(|| Mutex::new(HashMap::new()))
}
