//! ISIN (International Securities Identification Number, ISO 6166) validation
//! and field extraction as DuckDB scalar functions.
//!
//! Exposes four scalars, each taking a single VARCHAR:
//!   isin_validate(text)    -> BOOLEAN  true if the check digit is correct
//!   isin_check_digit(text) -> BIGINT   the expected Luhn check digit (0..9)
//!   isin_country(text)     -> VARCHAR  the 2-letter ISO country prefix
//!   isin_nsin(text)        -> VARCHAR  the 9-char national security identifier
//!
//! The check digit is a Luhn mod-10 over the body with letters expanded to
//! their A=10..Z=35 values. DB-agnostic logic shared with ~/git/sqlite-wasm's
//! `isin` extension; only the registration ABI differs.

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
            name: "isin".into(),
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

// ---- ISIN algorithm (DB-agnostic) ----

/// Strip whitespace + hyphens and upper-case.
fn normalize(s: &str) -> String {
    s.chars()
        .filter(|c| !c.is_whitespace() && *c != '-')
        .collect::<String>()
        .to_ascii_uppercase()
}

/// Expand each letter to its 2-digit value (A=10..Z=35) and each digit to
/// itself, concatenated. Returns None on any non-alphanumeric char.
fn expand(s: &str) -> Option<String> {
    let mut out = String::with_capacity(s.len() * 2);
    for c in s.chars() {
        if c.is_ascii_digit() {
            out.push(c);
        } else if c.is_ascii_alphabetic() {
            let v = (c.to_ascii_uppercase() as u32) - ('A' as u32) + 10;
            out.push_str(&format!("{v}"));
        } else {
            return None;
        }
    }
    Some(out)
}

/// Luhn check digit (0..9) over a digit-only string.
fn luhn_check_digit(s: &str) -> Option<u32> {
    let mut sum = 0u32;
    let mut alt = true;
    for c in s.chars().rev() {
        let d = c.to_digit(10)?;
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
    Some((10 - (sum % 10)) % 10)
}

fn expected_check_digit(normalized: &str) -> Option<u32> {
    if normalized.len() != 12 {
        return None;
    }
    expand(&normalized[..11]).as_deref().and_then(luhn_check_digit)
}

fn validate(normalized: &str) -> bool {
    if normalized.len() != 12 {
        return false;
    }
    let last = match normalized.as_bytes()[11] {
        b @ b'0'..=b'9' => (b - b'0') as u32,
        _ => return false,
    };
    matches!(expected_check_digit(normalized), Some(expected) if expected == last)
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

        let raw = arg_text(&args, 0, "isin")?;
        let n = normalize(&raw);

        Ok(match which {
            ScalarHandler::Validate => types::Duckvalue::Boolean(validate(&n)),
            ScalarHandler::CheckDigit => match expected_check_digit(&n) {
                Some(d) => types::Duckvalue::Int64(d as i64),
                None => types::Duckvalue::Null,
            },
            ScalarHandler::Country => {
                if n.len() == 12 {
                    types::Duckvalue::Text(n[..2].into())
                } else {
                    types::Duckvalue::Null
                }
            }
            ScalarHandler::Nsin => {
                if n.len() == 12 {
                    types::Duckvalue::Text(n[2..11].into())
                } else {
                    types::Duckvalue::Null
                }
            }
        })
    }

    fn call_table(
        _handle: u32,
        _args: Vec<types::Duckvalue>,
    ) -> Result<types::Resultset, types::Duckerror> {
        Err(types::Duckerror::Unsupported("isin: no table functions".into()))
    }

    fn call_aggregate(
        _handle: u32,
        _rows: types::Rowbatch,
    ) -> Result<types::Duckvalue, types::Duckerror> {
        Err(types::Duckerror::Unsupported("isin: no aggregates".into()))
    }

    fn call_pragma(
        _handle: u32,
        _args: Vec<types::Duckvalue>,
    ) -> Result<Option<types::Duckvalue>, types::Duckerror> {
        Err(types::Duckerror::Unsupported("isin: no pragmas".into()))
    }

    fn call_cast(
        _handle: u32,
        _value: types::Duckvalue,
    ) -> Result<types::Duckvalue, types::Duckerror> {
        Err(types::Duckerror::Unsupported("isin: no casts".into()))
    }
}

export!(Extension);

// ---- Registration ----

fn register_scalars() -> Result<(), types::Duckerror> {
    let capability = runtime::get_capability(types::Capabilitykind::Scalar)
        .ok_or_else(|| types::Duckerror::Internal("host did not expose scalar capability".into()))?;
    let registry = match capability {
        runtime::Capability::Scalar(registry) => registry,
        _ => {
            return Err(types::Duckerror::Internal(
                "scalar capability returned unexpected variant".into(),
            ))
        }
    };

    let det = types::Funcflags::DETERMINISTIC | types::Funcflags::STATELESS;
    register_one(&registry, "isin_validate", types::Logicaltype::Boolean, det, ScalarHandler::Validate)?;
    register_one(&registry, "isin_check_digit", types::Logicaltype::Int64, det, ScalarHandler::CheckDigit)?;
    register_one(&registry, "isin_country", types::Logicaltype::Text, det, ScalarHandler::Country)?;
    register_one(&registry, "isin_nsin", types::Logicaltype::Text, det, ScalarHandler::Nsin)?;
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
        description: Some("ISIN (ISO 6166) helper".into()),
        tags: vec!["isin".into()],
        attributes,
    };
    registry.register(name, &args, returns, callback, Some(&opts))?;
    Ok(())
}

#[derive(Clone, Copy)]
enum ScalarHandler {
    Validate,
    CheckDigit,
    Country,
    Nsin,
}

static NEXT_SCALAR_HANDLE: AtomicU32 = AtomicU32::new(1);
static SCALAR_HANDLERS: OnceLock<Mutex<HashMap<u32, ScalarHandler>>> = OnceLock::new();

fn scalar_handlers() -> &'static Mutex<HashMap<u32, ScalarHandler>> {
    SCALAR_HANDLERS.get_or_init(|| Mutex::new(HashMap::new()))
}
