//! IBAN (ISO 13616) validation + field extraction as DuckDB scalar functions.
//!
//!   iban_validate(text) -> BOOLEAN  true if the ISO 7064 mod-97 check passes
//!   iban_country(text)  -> VARCHAR  the 2-letter country prefix
//!   iban_bban(text)     -> VARCHAR  the Basic Bank Account Number (after pos 4)
//!
//! Whitespace and hyphens are ignored. Validation: length 15..34, letters[0..2]
//! + digits[2..4], move the first 4 chars to the end, expand letters (A=10..Z=35)
//! and reduce the resulting integer mod 97 == 1 (processed incrementally).

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
            name: "iban".into(),
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

// ---- IBAN algorithm (DB-agnostic) ----

fn normalize(s: &str) -> String {
    s.chars()
        .filter(|c| !c.is_whitespace() && *c != '-')
        .collect::<String>()
        .to_ascii_uppercase()
}

fn validate(s: &str) -> bool {
    let n = normalize(s);
    let b = n.as_bytes();
    if n.len() < 15 || n.len() > 34 {
        return false;
    }
    if !b[0].is_ascii_alphabetic() || !b[1].is_ascii_alphabetic()
        || !b[2].is_ascii_digit() || !b[3].is_ascii_digit()
    {
        return false;
    }
    // rearrange: first four chars to the end
    let mut rem: u32 = 0;
    let mut feed = |c: char| -> bool {
        if c.is_ascii_digit() {
            rem = (rem * 10 + (c as u32 - '0' as u32)) % 97;
            true
        } else if c.is_ascii_alphabetic() {
            let v = c as u32 - 'A' as u32 + 10; // 10..35 -> two digits
            rem = (rem * 10 + v / 10) % 97;
            rem = (rem * 10 + v % 10) % 97;
            true
        } else {
            false
        }
    };
    for c in n[4..].chars().chain(n[..4].chars()) {
        if !feed(c) {
            return false;
        }
    }
    rem == 1
}

fn country(s: &str) -> Option<String> {
    let n = normalize(s);
    let b = n.as_bytes();
    if n.len() >= 2 && b[0].is_ascii_alphabetic() && b[1].is_ascii_alphabetic() {
        Some(n[..2].into())
    } else {
        None
    }
}

fn bban(s: &str) -> Option<String> {
    let n = normalize(s);
    if n.len() > 4 {
        Some(n[4..].into())
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
        let raw = arg_text(&args, 0, "iban")?;
        Ok(match which {
            ScalarHandler::Validate => types::Duckvalue::Boolean(validate(&raw)),
            ScalarHandler::Country => match country(&raw) {
                Some(c) => types::Duckvalue::Text(c),
                None => types::Duckvalue::Null,
            },
            ScalarHandler::Bban => match bban(&raw) {
                Some(b) => types::Duckvalue::Text(b),
                None => types::Duckvalue::Null,
            },
        })
    }

    fn call_table(
        _handle: u32,
        _args: Vec<types::Duckvalue>,
    ) -> Result<types::Resultset, types::Duckerror> {
        Err(types::Duckerror::Unsupported("iban: no table functions".into()))
    }
    fn call_aggregate(
        _handle: u32,
        _rows: types::Rowbatch,
    ) -> Result<types::Duckvalue, types::Duckerror> {
        Err(types::Duckerror::Unsupported("iban: no aggregates".into()))
    }
    fn call_pragma(
        _handle: u32,
        _args: Vec<types::Duckvalue>,
    ) -> Result<Option<types::Duckvalue>, types::Duckerror> {
        Err(types::Duckerror::Unsupported("iban: no pragmas".into()))
    }
    fn call_cast(
        _handle: u32,
        _value: types::Duckvalue,
    ) -> Result<types::Duckvalue, types::Duckerror> {
        Err(types::Duckerror::Unsupported("iban: no casts".into()))
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
    register_one(&registry, "iban_validate", types::Logicaltype::Boolean, det, ScalarHandler::Validate)?;
    register_one(&registry, "iban_country", types::Logicaltype::Text, det, ScalarHandler::Country)?;
    register_one(&registry, "iban_bban", types::Logicaltype::Text, det, ScalarHandler::Bban)?;
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
        description: Some("IBAN (ISO 13616) helper".into()),
        tags: vec!["iban".into()],
        attributes,
    };
    registry.register(name, &args, returns, callback, Some(&opts))?;
    Ok(())
}

#[derive(Clone, Copy)]
enum ScalarHandler {
    Validate,
    Country,
    Bban,
}

static NEXT_SCALAR_HANDLE: AtomicU32 = AtomicU32::new(1);
static SCALAR_HANDLERS: OnceLock<Mutex<HashMap<u32, ScalarHandler>>> = OnceLock::new();

fn scalar_handlers() -> &'static Mutex<HashMap<u32, ScalarHandler>> {
    SCALAR_HANDLERS.get_or_init(|| Mutex::new(HashMap::new()))
}
