//! Number formatting as DuckDB scalars (hand-rolled, no upstream crates):
//!   num_group(value, decimals) -> text  fixed decimals + comma thousands-grouping,
//!     e.g. num_group(1234567.5, 2) -> "1,234,567.50".
//!   num_si(value) -> text  metric/SI prefix at 3 significant figures,
//!     e.g. num_si(1500) -> "1.5k", num_si(2300000) -> "2.3M", num_si(0.0023) -> "2.3m".
//!   NULL / non-finite input -> NULL. Never panics.
use std::collections::HashMap;
use std::sync::{
    atomic::{AtomicU32, Ordering},
    Mutex, OnceLock,
};
use wit_bindgen::rt::string::String;
use wit_bindgen::rt::vec::Vec;
wit_bindgen::generate!({ path: "./wit", world: "duckdb:extension/duckdb-extension" });
use duckdb::extension::{runtime, types};
use exports::duckdb::extension::{callback_dispatch, guest};

struct Extension;

impl guest::Guest for Extension {
    fn load() -> Result<types::Loadresult, types::Duckerror> {
        register_scalars()?;
        Ok(types::Loadresult {
            name: "numfmt".into(),
            version: Some(env!("CARGO_PKG_VERSION").into()),
            requires: Vec::new().into(),
        })
    }
    fn reconfigure(_k: Vec<String>) -> Result<bool, types::Duckerror> {
        Ok(false)
    }
    fn shutdown() -> Result<bool, types::Duckerror> {
        Ok(false)
    }
}

fn f64_arg(args: &[types::Duckvalue], i: usize) -> Option<f64> {
    match args.get(i) {
        Some(types::Duckvalue::Float64(v)) => Some(*v),
        Some(types::Duckvalue::Int64(v)) => Some(*v as f64),
        Some(types::Duckvalue::Uint64(v)) => Some(*v as f64),
        _ => None,
    }
}

fn i64_arg(args: &[types::Duckvalue], i: usize) -> Option<i64> {
    match args.get(i) {
        Some(types::Duckvalue::Int64(v)) => Some(*v),
        Some(types::Duckvalue::Uint64(v)) => Some(*v as i64),
        Some(types::Duckvalue::Float64(v)) => Some(*v as i64),
        _ => None,
    }
}

/// Insert commas as thousands separators into the (already-sign-stripped) integer
/// part string, e.g. "1234567" -> "1,234,567".
fn group_int(int_part: &str) -> std::string::String {
    let bytes = int_part.as_bytes();
    let n = bytes.len();
    let mut out = std::string::String::with_capacity(n + n / 3);
    for (idx, &b) in bytes.iter().enumerate() {
        if idx > 0 && (n - idx) % 3 == 0 {
            out.push(',');
        }
        out.push(b as char);
    }
    out
}

/// num_group: fixed `decimals` places, comma thousands-grouping.
fn num_group(value: f64, decimals: i64) -> Option<std::string::String> {
    if !value.is_finite() {
        return None;
    }
    let dec = decimals.clamp(0, 30) as usize;
    // Format with the requested decimals; this rounds half-to-even per Rust's
    // float formatting, then we group the integer portion.
    let formatted = format!("{:.*}", dec, value.abs());
    let (int_part, frac_part) = match formatted.split_once('.') {
        Some((i, f)) => (i, Some(f)),
        None => (formatted.as_str(), None),
    };
    let mut out = std::string::String::new();
    if value.is_sign_negative() && (int_part.bytes().any(|b| b != b'0') || frac_has_nonzero(frac_part)) {
        out.push('-');
    }
    out.push_str(&group_int(int_part));
    if let Some(f) = frac_part {
        out.push('.');
        out.push_str(f);
    }
    Some(out)
}

fn frac_has_nonzero(frac: Option<&str>) -> bool {
    frac.map_or(false, |f| f.bytes().any(|b| b != b'0'))
}

/// num_si: metric/SI prefix at 3 significant figures.
/// e.g. 1500 -> "1.5k", 2300000 -> "2.3M", 0.0023 -> "2.3m", 0 -> "0".
fn num_si(value: f64) -> Option<std::string::String> {
    if !value.is_finite() {
        return None;
    }
    if value == 0.0 {
        return Some("0".into());
    }
    // Prefixes indexed by power-of-1000 exponent.
    const POS: [&str; 9] = ["", "k", "M", "G", "T", "P", "E", "Z", "Y"];
    const NEG: [&str; 9] = ["", "m", "u", "n", "p", "f", "a", "z", "y"];

    let neg = value < 0.0;
    let mut mag = value.abs();
    let mut exp: i32 = 0;
    if mag >= 1000.0 {
        while mag >= 1000.0 && exp < (POS.len() as i32 - 1) {
            mag /= 1000.0;
            exp += 1;
        }
    } else if mag < 1.0 {
        while mag < 1.0 && exp > -(NEG.len() as i32 - 1) {
            mag *= 1000.0;
            exp -= 1;
        }
    }

    // 3 significant figures on the mantissa (1.00 .. 999).
    let decimals = if mag >= 100.0 {
        0
    } else if mag >= 10.0 {
        1
    } else {
        2
    };
    let mut mant = format!("{:.*}", decimals, mag);
    // Rounding can push e.g. 999.5 -> "1000"; renormalize one step if so.
    if mant.starts_with("1000") && exp < (POS.len() as i32 - 1) {
        mag /= 1000.0;
        exp += 1;
        mant = format!("{:.2}", mag);
    }
    // Trim trailing zeros / dot for a clean "1.5k" rather than "1.50k".
    if mant.contains('.') {
        let trimmed = mant.trim_end_matches('0').trim_end_matches('.');
        mant = trimmed.to_string();
    }

    let prefix = if exp >= 0 {
        POS[exp as usize]
    } else {
        NEG[(-exp) as usize]
    };
    let mut out = std::string::String::new();
    if neg {
        out.push('-');
    }
    out.push_str(&mant);
    out.push_str(prefix);
    Some(out)
}

impl callback_dispatch::Guest for Extension {
    fn call_scalar_batch(
        h: u32,
        rows: Vec<Vec<types::Duckvalue>>,
        ctx: types::Invokeinfo,
    ) -> Result<Vec<types::Duckvalue>, types::Duckerror> {
        let base = ctx.rowindex.unwrap_or(0);
        let mut out = Vec::with_capacity(rows.len());
        for (i, a) in rows.into_iter().enumerate() {
            out.push(Self::call_scalar(
                h,
                a,
                types::Invokeinfo {
                    rowindex: Some(base + i as u64),
                    iswindow: ctx.iswindow,
                },
            )?);
        }
        Ok(out)
    }
    fn call_scalar(
        handle: u32,
        args: Vec<types::Duckvalue>,
        _c: types::Invokeinfo,
    ) -> Result<types::Duckvalue, types::Duckerror> {
        let which = handlers()
            .lock()
            .unwrap()
            .get(&handle)
            .copied()
            .ok_or_else(|| types::Duckerror::Internal("unknown scalar handle".into()))?;
        Ok(match which {
            G::Group => {
                let v = f64_arg(&args, 0);
                let d = i64_arg(&args, 1).unwrap_or(0);
                match v.and_then(|v| num_group(v, d)) {
                    Some(s) => types::Duckvalue::Text(s.into()),
                    None => types::Duckvalue::Null,
                }
            }
            G::Si => match f64_arg(&args, 0).and_then(num_si) {
                Some(s) => types::Duckvalue::Text(s.into()),
                None => types::Duckvalue::Null,
            },
        })
    }
    fn call_table(
        _h: u32,
        _a: Vec<types::Duckvalue>,
    ) -> Result<types::Resultset, types::Duckerror> {
        Err(types::Duckerror::Unsupported("numfmt: no table fns".into()))
    }
    fn call_aggregate(
        _h: u32,
        _r: types::Rowbatch,
    ) -> Result<types::Duckvalue, types::Duckerror> {
        Err(types::Duckerror::Unsupported("numfmt: no aggs".into()))
    }
    fn call_pragma(
        _h: u32,
        _a: Vec<types::Duckvalue>,
    ) -> Result<Option<types::Duckvalue>, types::Duckerror> {
        Err(types::Duckerror::Unsupported("numfmt: no pragmas".into()))
    }
    fn call_cast(
        _h: u32,
        _v: types::Duckvalue,
    ) -> Result<types::Duckvalue, types::Duckerror> {
        Err(types::Duckerror::Unsupported("numfmt: no casts".into()))
    }
}
export!(Extension);

fn register_scalars() -> Result<(), types::Duckerror> {
    let cap = runtime::get_capability(types::Capabilitykind::Scalar)
        .ok_or_else(|| types::Duckerror::Internal("no scalar capability".into()))?;
    let reg = match cap {
        runtime::Capability::Scalar(r) => r,
        _ => return Err(types::Duckerror::Internal("bad capability".into())),
    };
    let det = types::Funcflags::DETERMINISTIC | types::Funcflags::STATELESS;

    let h = NEXT.fetch_add(1, Ordering::Relaxed);
    handlers().lock().unwrap().insert(h, G::Group);
    reg.register(
        "num_group",
        &[
            runtime::Funcarg {
                name: Some("value".into()),
                logical: types::Logicaltype::Float64,
            },
            runtime::Funcarg {
                name: Some("decimals".into()),
                logical: types::Logicaltype::Int64,
            },
        ],
        types::Logicaltype::Text,
        runtime::ScalarCallback::new(h),
        Some(&runtime::Funcopts {
            description: Some("value -> fixed decimals with comma thousands-grouping".into()),
            tags: vec!["number".into(), "format".into()],
            attributes: det,
        }),
    )?;

    let h = NEXT.fetch_add(1, Ordering::Relaxed);
    handlers().lock().unwrap().insert(h, G::Si);
    reg.register(
        "num_si",
        &[runtime::Funcarg {
            name: Some("value".into()),
            logical: types::Logicaltype::Float64,
        }],
        types::Logicaltype::Text,
        runtime::ScalarCallback::new(h),
        Some(&runtime::Funcopts {
            description: Some("value -> SI/metric prefixed string (3 sig figs)".into()),
            tags: vec!["number".into(), "format".into(), "si".into()],
            attributes: det,
        }),
    )?;
    Ok(())
}

#[derive(Clone, Copy, PartialEq)]
enum G {
    Group,
    Si,
}
static NEXT: AtomicU32 = AtomicU32::new(1);
static HANDLERS: OnceLock<Mutex<HashMap<u32, G>>> = OnceLock::new();
fn handlers() -> &'static Mutex<HashMap<u32, G>> {
    HANDLERS.get_or_init(|| Mutex::new(HashMap::new()))
}
