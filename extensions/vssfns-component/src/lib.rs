//! The genuinely-missing vector math from DuckDB's `vss` extension, as DuckDB
//! scalars. The WIT value surface is scalar-only (no ARRAY/LIST), so vectors are
//! passed as JSON number arrays in VARCHAR:
//!
//!   vec_l1_distance(a, b)   -> DOUBLE   Manhattan / L1 distance
//!   vec_linf_distance(a, b) -> DOUBLE   Chebyshev / L-infinity distance
//!   vec_normalize(a)        -> VARCHAR  unit vector (JSON array), L2-normalized
//!
//! vss's L2 / cosine / inner-product distances are ALREADY in lean
//! core_functions (array_distance / array_cosine_distance / array_inner_product
//! / array_dot_product and the list_ variants), so they are deliberately NOT
//! reimplemented here — doing so would collide. Everything is NULL-safe and
//! never panics: NULL in, bad JSON, length mismatch, or non-finite -> NULL.
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

#[derive(Clone, Copy, PartialEq)]
enum F {
    L1,
    Linf,
    Normalize,
}

impl guest::Guest for Extension {
    fn load() -> Result<types::Loadresult, types::Duckerror> {
        register_scalars()?;
        Ok(types::Loadresult {
            name: "vssfns".into(),
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

fn text_arg(args: &[types::Duckvalue], i: usize) -> Option<String> {
    match args.get(i) {
        Some(types::Duckvalue::Text(s)) => Some(s.clone()),
        _ => None,
    }
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
            F::L1 | F::Linf => {
                let r = text_arg(&args, 0)
                    .zip(text_arg(&args, 1))
                    .and_then(|(a, b)| pair_distance(&a, &b, which));
                match r {
                    Some(d) => types::Duckvalue::Float64(d),
                    None => types::Duckvalue::Null,
                }
            }
            F::Normalize => match text_arg(&args, 0).and_then(|a| normalize(&a)) {
                Some(s) => types::Duckvalue::Text(s.into()),
                None => types::Duckvalue::Null,
            },
        })
    }

    fn call_table(
        _h: u32,
        _a: Vec<types::Duckvalue>,
    ) -> Result<types::Resultset, types::Duckerror> {
        Err(types::Duckerror::Unsupported("vssfns: no table fns".into()))
    }
    fn call_aggregate(
        _h: u32,
        _r: types::Rowbatch,
    ) -> Result<types::Duckvalue, types::Duckerror> {
        Err(types::Duckerror::Unsupported("vssfns: no aggs".into()))
    }
    fn call_pragma(
        _h: u32,
        _a: Vec<types::Duckvalue>,
    ) -> Result<Option<types::Duckvalue>, types::Duckerror> {
        Err(types::Duckerror::Unsupported("vssfns: no pragmas".into()))
    }
    fn call_cast(
        _h: u32,
        _v: types::Duckvalue,
    ) -> Result<types::Duckvalue, types::Duckerror> {
        Err(types::Duckerror::Unsupported("vssfns: no casts".into()))
    }
}

export!(Extension);

// ---- pure vector math (unit-tested below) -----------------------------------

/// Parse a JSON array of finite numbers into `Vec<f64>`. Any non-array, any
/// non-number element, or any non-finite (NaN/Inf) element -> None.
fn parse_vec(s: &str) -> Option<std::vec::Vec<f64>> {
    let v: serde_json::Value = serde_json::from_str(s).ok()?;
    let arr = v.as_array()?;
    let mut out = std::vec::Vec::with_capacity(arr.len());
    for e in arr {
        let x = e.as_f64()?;
        if !x.is_finite() {
            return None;
        }
        out.push(x);
    }
    Some(out)
}

/// L1 (Manhattan) or L-infinity (Chebyshev) distance between two equal-length
/// vectors. Length mismatch, empty, parse failure, or non-finite -> None.
fn pair_distance(a: &str, b: &str, which: F) -> Option<f64> {
    let a = parse_vec(a)?;
    let b = parse_vec(b)?;
    if a.len() != b.len() || a.is_empty() {
        return None;
    }
    let d = match which {
        F::L1 => a.iter().zip(&b).map(|(x, y)| (x - y).abs()).sum(),
        F::Linf => a
            .iter()
            .zip(&b)
            .map(|(x, y)| (x - y).abs())
            .fold(0.0f64, f64::max),
        F::Normalize => unreachable!(),
    };
    if d.is_finite() {
        Some(d)
    } else {
        None
    }
}

/// L2-normalize a vector to a unit vector, rendered as a JSON array string.
/// Parse failure, empty, or zero/near-zero magnitude -> None.
fn normalize(a: &str) -> Option<String> {
    let v = parse_vec(a)?;
    if v.is_empty() {
        return None;
    }
    let norm = v.iter().map(|x| x * x).sum::<f64>().sqrt();
    if !norm.is_finite() || norm == 0.0 {
        return None;
    }
    let unit: std::vec::Vec<f64> = v.iter().map(|x| x / norm).collect();
    if unit.iter().any(|x| !x.is_finite()) {
        return None;
    }
    serde_json::to_string(&unit).ok().map(Into::into)
}

// ---- registration -----------------------------------------------------------

fn register_scalars() -> Result<(), types::Duckerror> {
    let cap = runtime::get_capability(types::Capabilitykind::Scalar)
        .ok_or_else(|| types::Duckerror::Internal("no scalar capability".into()))?;
    let reg = match cap {
        runtime::Capability::Scalar(r) => r,
        _ => return Err(types::Duckerror::Internal("bad capability".into())),
    };
    let det = types::Funcflags::DETERMINISTIC | types::Funcflags::STATELESS;

    let dist_arg = |n: &str| runtime::Funcarg {
        name: Some(n.into()),
        logical: types::Logicaltype::Text,
    };

    for (name, f, desc) in [
        (
            "vec_l1_distance",
            F::L1,
            "Manhattan/L1 distance between two JSON number arrays",
        ),
        (
            "vec_linf_distance",
            F::Linf,
            "Chebyshev/L-infinity distance between two JSON number arrays",
        ),
    ] {
        let h = NEXT.fetch_add(1, Ordering::Relaxed);
        handlers().lock().unwrap().insert(h, f);
        reg.register(
            name,
            &[dist_arg("a"), dist_arg("b")],
            types::Logicaltype::Float64,
            runtime::ScalarCallback::new(h),
            Some(&runtime::Funcopts {
                description: Some(desc.into()),
                tags: vec!["vector".into()],
                attributes: det,
            }),
        )?;
    }

    let h = NEXT.fetch_add(1, Ordering::Relaxed);
    handlers().lock().unwrap().insert(h, F::Normalize);
    reg.register(
        "vec_normalize",
        &[dist_arg("a")],
        types::Logicaltype::Text,
        runtime::ScalarCallback::new(h),
        Some(&runtime::Funcopts {
            description: Some("L2-normalize a JSON number array to a unit vector".into()),
            tags: vec!["vector".into()],
            attributes: det,
        }),
    )?;
    Ok(())
}

static NEXT: AtomicU32 = AtomicU32::new(1);
static HANDLERS: OnceLock<Mutex<HashMap<u32, F>>> = OnceLock::new();
fn handlers() -> &'static Mutex<HashMap<u32, F>> {
    HANDLERS.get_or_init(|| Mutex::new(HashMap::new()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn l1() {
        let d = pair_distance("[1, 2, 3]", "[4, 6, 3]", F::L1).unwrap();
        assert!((d - 7.0).abs() < 1e-9, "got {d}"); // |1-4|+|2-6|+|3-3| = 3+4+0
    }

    #[test]
    fn linf() {
        let d = pair_distance("[1, 2, 3]", "[4, 6, 3]", F::Linf).unwrap();
        assert!((d - 4.0).abs() < 1e-9, "got {d}"); // max(3,4,0)
    }

    #[test]
    fn distance_zero_for_equal() {
        assert_eq!(pair_distance("[1,2]", "[1,2]", F::L1), Some(0.0));
        assert_eq!(pair_distance("[1,2]", "[1,2]", F::Linf), Some(0.0));
    }

    #[test]
    fn distance_length_mismatch_is_none() {
        assert_eq!(pair_distance("[1,2,3]", "[1,2]", F::L1), None);
    }

    #[test]
    fn distance_bad_json_is_none() {
        assert_eq!(pair_distance("not json", "[1,2]", F::L1), None);
        assert_eq!(pair_distance("[1,\"x\"]", "[1,2]", F::L1), None);
        assert_eq!(pair_distance("{}", "[1,2]", F::L1), None);
    }

    #[test]
    fn distance_empty_is_none() {
        assert_eq!(pair_distance("[]", "[]", F::L1), None);
    }

    #[test]
    fn distance_overflow_to_inf_is_none() {
        // |1e308 - (-1e308)| = 2e308 each, summed -> +Inf -> finite check -> None.
        assert_eq!(pair_distance("[1e308, 1e308]", "[-1e308, -1e308]", F::L1), None);
    }

    #[test]
    fn normalize_basic() {
        // [3,4] -> norm 5 -> [0.6, 0.8]
        let s = normalize("[3, 4]").unwrap();
        let v: std::vec::Vec<f64> = serde_json::from_str(&s).unwrap();
        assert!((v[0] - 0.6).abs() < 1e-9 && (v[1] - 0.8).abs() < 1e-9, "got {s}");
        // magnitude is 1
        let mag = (v[0] * v[0] + v[1] * v[1]).sqrt();
        assert!((mag - 1.0).abs() < 1e-9);
    }

    #[test]
    fn normalize_zero_vector_is_none() {
        assert_eq!(normalize("[0, 0, 0]"), None);
    }

    #[test]
    fn normalize_bad_json_is_none() {
        assert_eq!(normalize("nope"), None);
        assert_eq!(normalize("[]"), None);
    }
}
