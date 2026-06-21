//! Credit-card number validation + network detection as DuckDB scalars.
//!
//!   cc_validate(text) -> BOOLEAN  Luhn-valid AND a plausible length (12..19)
//!   cc_network(text)  -> VARCHAR  the card network by IIN prefix, or NULL
//!
//! Whitespace and hyphens are ignored.

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
            name: "creditcard".into(),
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

fn luhn_ok(num: &[u32]) -> bool {
    let mut sum = 0u32;
    let mut alt = false;
    for &d in num.iter().rev() {
        let v = if alt {
            let x = d * 2;
            if x > 9 {
                x - 9
            } else {
                x
            }
        } else {
            d
        };
        sum += v;
        alt = !alt;
    }
    sum % 10 == 0
}

fn validate(num: &[u32]) -> bool {
    (12..=19).contains(&num.len()) && luhn_ok(num)
}

/// First `k` digits as an integer (for prefix matching).
fn prefix(num: &[u32], k: usize) -> u32 {
    num.iter().take(k).fold(0, |a, &d| a * 10 + d)
}

fn network(num: &[u32]) -> Option<&'static str> {
    if num.len() < 12 {
        return None;
    }
    let len = num.len();
    let p2 = prefix(num, 2);
    let p3 = prefix(num, 3);
    let p4 = prefix(num, 4);
    let p6 = prefix(num, 6);
    if num[0] == 4 {
        Some("Visa")
    } else if (51..=55).contains(&p2) || (2221..=2720).contains(&p4) {
        Some("Mastercard")
    } else if p2 == 34 || p2 == 37 {
        Some("American Express")
    } else if p4 == 6011 || p2 == 65 || (644..=649).contains(&p3)
        || (622126..=622925).contains(&p6)
    {
        Some("Discover")
    } else if (3528..=3589).contains(&p4) {
        Some("JCB")
    } else if (300..=305).contains(&p3) || p3 == 309 || p2 == 36 || p2 == 38 || p2 == 39 {
        Some("Diners Club")
    } else if p2 == 62 || p2 == 81 {
        Some("UnionPay")
    } else if (p2 == 50 || (56..=69).contains(&p2)) && len >= 12 {
        Some("Maestro")
    } else {
        None
    }
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
        let raw = arg_text(&args, 0, "creditcard")?;
        Ok(match which {
            ScalarHandler::Validate => {
                types::Duckvalue::Boolean(digits(&raw).map(|d| validate(&d)).unwrap_or(false))
            }
            ScalarHandler::Network => match digits(&raw).and_then(|d| network(&d)) {
                Some(n) => types::Duckvalue::Text(n.into()),
                None => types::Duckvalue::Null,
            },
        })
    }

    fn call_table(
        _handle: u32,
        _args: Vec<types::Duckvalue>,
    ) -> Result<types::Resultset, types::Duckerror> {
        Err(types::Duckerror::Unsupported("creditcard: no table functions".into()))
    }
    fn call_aggregate(
        _handle: u32,
        _rows: types::Rowbatch,
    ) -> Result<types::Duckvalue, types::Duckerror> {
        Err(types::Duckerror::Unsupported("creditcard: no aggregates".into()))
    }
    fn call_pragma(
        _handle: u32,
        _args: Vec<types::Duckvalue>,
    ) -> Result<Option<types::Duckvalue>, types::Duckerror> {
        Err(types::Duckerror::Unsupported("creditcard: no pragmas".into()))
    }
    fn call_cast(
        _handle: u32,
        _value: types::Duckvalue,
    ) -> Result<types::Duckvalue, types::Duckerror> {
        Err(types::Duckerror::Unsupported("creditcard: no casts".into()))
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
    register_one(&registry, "cc_validate", types::Logicaltype::Boolean, det, ScalarHandler::Validate)?;
    register_one(&registry, "cc_network", types::Logicaltype::Text, det, ScalarHandler::Network)?;
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
        description: Some("Credit-card number helper".into()),
        tags: vec!["creditcard".into()],
        attributes,
    };
    registry.register(name, &args, returns, callback, Some(&opts))?;
    Ok(())
}

#[derive(Clone, Copy)]
enum ScalarHandler {
    Validate,
    Network,
}

static NEXT_SCALAR_HANDLE: AtomicU32 = AtomicU32::new(1);
static SCALAR_HANDLERS: OnceLock<Mutex<HashMap<u32, ScalarHandler>>> = OnceLock::new();

fn scalar_handlers() -> &'static Mutex<HashMap<u32, ScalarHandler>> {
    SCALAR_HANDLERS.get_or_init(|| Mutex::new(HashMap::new()))
}
