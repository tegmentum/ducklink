//! Astronomical coordinate conversions as DuckDB scalars (hand-rolled math):
//!   equatorial_to_galactic_l(ra_deg, dec_deg) -> double,
//!   equatorial_to_galactic_b(ra_deg, dec_deg) -> double,
//!   angular_separation(ra1, dec1, ra2, dec2) -> double,
//!   hms_to_deg(h, m, s) -> double, dms_to_deg(d, m, s) -> double.
//! NULL on NULL input; never panics.
use std::collections::HashMap;
use std::sync::{atomic::{AtomicU32, Ordering}, Mutex, OnceLock};
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
            name: "celestial".into(),
            version: Some(env!("CARGO_PKG_VERSION").into()),
            requires: Vec::new().into(),
        })
    }
    fn reconfigure(_k: Vec<String>) -> Result<bool, types::Duckerror> { Ok(false) }
    fn shutdown() -> Result<bool, types::Duckerror> { Ok(false) }
}

/// Accepts FLOAT64 or INT64; NULL / missing -> None (propagates to NULL output).
fn f64_arg(args: &[types::Duckvalue], i: usize) -> Option<f64> {
    match args.get(i) {
        Some(types::Duckvalue::Float64(v)) => Some(*v),
        Some(types::Duckvalue::Int64(v)) => Some(*v as f64),
        _ => None,
    }
}

// --- spherical astronomy (J2000) -------------------------------------------
//
// Galactic-pole constants in the J2000 frame (IAU 1958, precessed to J2000):
//   North Galactic Pole:  RA = 192.85948 deg, Dec = +27.12825 deg
//   Galactic longitude of the North Celestial Pole: l_NCP = 122.93192 deg
const RA_NGP_DEG: f64 = 192.85948;
const DEC_NGP_DEG: f64 = 27.12825;
const L_NCP_DEG: f64 = 122.93192;

const DEG2RAD: f64 = std::f64::consts::PI / 180.0;
const RAD2DEG: f64 = 180.0 / std::f64::consts::PI;

fn galactic_l(ra_deg: f64, dec_deg: f64) -> f64 {
    let ra = ra_deg * DEG2RAD;
    let dec = dec_deg * DEG2RAD;
    let ra_ngp = RA_NGP_DEG * DEG2RAD;
    let dec_ngp = DEC_NGP_DEG * DEG2RAD;
    // l = l_NCP - atan2( cos(dec) sin(ra - ra_ngp),
    //                    sin(dec) cos(dec_ngp) - cos(dec) sin(dec_ngp) cos(ra - ra_ngp) )
    let y = dec.cos() * (ra - ra_ngp).sin();
    let x = dec.sin() * dec_ngp.cos() - dec.cos() * dec_ngp.sin() * (ra - ra_ngp).cos();
    let mut l = L_NCP_DEG - y.atan2(x) * RAD2DEG;
    l = l.rem_euclid(360.0);
    l
}

fn galactic_b(ra_deg: f64, dec_deg: f64) -> f64 {
    let ra = ra_deg * DEG2RAD;
    let dec = dec_deg * DEG2RAD;
    let ra_ngp = RA_NGP_DEG * DEG2RAD;
    let dec_ngp = DEC_NGP_DEG * DEG2RAD;
    // sin(b) = sin(dec) sin(dec_ngp) + cos(dec) cos(dec_ngp) cos(ra - ra_ngp)
    let sin_b = dec.sin() * dec_ngp.sin() + dec.cos() * dec_ngp.cos() * (ra - ra_ngp).cos();
    sin_b.clamp(-1.0, 1.0).asin() * RAD2DEG
}

/// Great-circle angle between two equatorial points (degrees), haversine form.
fn angular_separation(ra1: f64, dec1: f64, ra2: f64, dec2: f64) -> f64 {
    let d1 = dec1 * DEG2RAD;
    let d2 = dec2 * DEG2RAD;
    let dra = (ra2 - ra1) * DEG2RAD;
    let ddec = d2 - d1;
    let h = (ddec / 2.0).sin().powi(2)
        + d1.cos() * d2.cos() * (dra / 2.0).sin().powi(2);
    2.0 * h.sqrt().clamp(0.0, 1.0).asin() * RAD2DEG
}

/// Hours-minutes-seconds of RA -> degrees (15 deg per hour). Sign from `h`.
fn hms_to_deg(h: f64, m: f64, s: f64) -> f64 {
    let sign = if h.is_sign_negative() { -1.0 } else { 1.0 };
    sign * (h.abs() + m / 60.0 + s / 3600.0) * 15.0
}

/// Degrees-minutes-seconds -> decimal degrees. Sign from `d`.
fn dms_to_deg(d: f64, m: f64, s: f64) -> f64 {
    let sign = if d.is_sign_negative() { -1.0 } else { 1.0 };
    sign * (d.abs() + m / 60.0 + s / 3600.0)
}

impl callback_dispatch::Guest for Extension {
    fn call_scalar_batch(h: u32, rows: Vec<Vec<types::Duckvalue>>, ctx: types::Invokeinfo) -> Result<Vec<types::Duckvalue>, types::Duckerror> {
        let base = ctx.rowindex.unwrap_or(0);
        let mut out = Vec::with_capacity(rows.len());
        for (i, a) in rows.into_iter().enumerate() {
            out.push(Self::call_scalar(h, a, types::Invokeinfo { rowindex: Some(base + i as u64), iswindow: ctx.iswindow })?);
        }
        Ok(out)
    }
    fn call_scalar(handle: u32, args: Vec<types::Duckvalue>, _c: types::Invokeinfo) -> Result<types::Duckvalue, types::Duckerror> {
        let which = handlers().lock().unwrap().get(&handle).copied()
            .ok_or_else(|| types::Duckerror::Internal("unknown scalar handle".into()))?;
        let null = types::Duckvalue::Null;
        Ok(match which {
            F::GalL => match (f64_arg(&args, 0), f64_arg(&args, 1)) {
                (Some(ra), Some(dec)) => types::Duckvalue::Float64(galactic_l(ra, dec)),
                _ => null,
            },
            F::GalB => match (f64_arg(&args, 0), f64_arg(&args, 1)) {
                (Some(ra), Some(dec)) => types::Duckvalue::Float64(galactic_b(ra, dec)),
                _ => null,
            },
            F::Sep => match (f64_arg(&args, 0), f64_arg(&args, 1), f64_arg(&args, 2), f64_arg(&args, 3)) {
                (Some(ra1), Some(dec1), Some(ra2), Some(dec2)) =>
                    types::Duckvalue::Float64(angular_separation(ra1, dec1, ra2, dec2)),
                _ => null,
            },
            F::Hms => match (f64_arg(&args, 0), f64_arg(&args, 1), f64_arg(&args, 2)) {
                (Some(h), Some(m), Some(s)) => types::Duckvalue::Float64(hms_to_deg(h, m, s)),
                _ => null,
            },
            F::Dms => match (f64_arg(&args, 0), f64_arg(&args, 1), f64_arg(&args, 2)) {
                (Some(d), Some(m), Some(s)) => types::Duckvalue::Float64(dms_to_deg(d, m, s)),
                _ => null,
            },
        })
    }
    fn call_table(_h: u32, _a: Vec<types::Duckvalue>) -> Result<types::Resultset, types::Duckerror> { Err(types::Duckerror::Unsupported("celestial: no table fns".into())) }
    fn call_aggregate(_h: u32, _r: types::Rowbatch) -> Result<types::Duckvalue, types::Duckerror> { Err(types::Duckerror::Unsupported("celestial: no aggs".into())) }
    fn call_pragma(_h: u32, _a: Vec<types::Duckvalue>) -> Result<Option<types::Duckvalue>, types::Duckerror> { Err(types::Duckerror::Unsupported("celestial: no pragmas".into())) }
    fn call_cast(_h: u32, _v: types::Duckvalue) -> Result<types::Duckvalue, types::Duckerror> { Err(types::Duckerror::Unsupported("celestial: no casts".into())) }
}
export!(Extension);

fn register_scalars() -> Result<(), types::Duckerror> {
    let cap = runtime::get_capability(types::Capabilitykind::Scalar)
        .ok_or_else(|| types::Duckerror::Internal("no scalar capability".into()))?;
    let reg = match cap { runtime::Capability::Scalar(r) => r, _ => return Err(types::Duckerror::Internal("bad capability".into())) };
    let det = types::Funcflags::DETERMINISTIC | types::Funcflags::STATELESS;
    let dbl = types::Logicaltype::Float64;
    let arg = |n: &str| runtime::Funcarg { name: Some(n.into()), logical: types::Logicaltype::Float64 };
    let opts = |d: &str| runtime::Funcopts { description: Some(d.into()), tags: vec!["astro".into()], attributes: det };

    let h = next(F::GalL);
    reg.register("equatorial_to_galactic_l",
        &[arg("ra_deg"), arg("dec_deg")], &dbl, runtime::ScalarCallback::new(h),
        Some(&opts("RA/Dec (J2000) -> galactic longitude (deg)")))?;

    let h = next(F::GalB);
    reg.register("equatorial_to_galactic_b",
        &[arg("ra_deg"), arg("dec_deg")], &dbl, runtime::ScalarCallback::new(h),
        Some(&opts("RA/Dec (J2000) -> galactic latitude (deg)")))?;

    let h = next(F::Sep);
    reg.register("angular_separation",
        &[arg("ra1"), arg("dec1"), arg("ra2"), arg("dec2")], &dbl, runtime::ScalarCallback::new(h),
        Some(&opts("Great-circle angle between two equatorial points (deg)")))?;

    let h = next(F::Hms);
    reg.register("hms_to_deg",
        &[arg("h"), arg("m"), arg("s")], &dbl, runtime::ScalarCallback::new(h),
        Some(&opts("Hours-minutes-seconds of RA -> degrees")))?;

    let h = next(F::Dms);
    reg.register("dms_to_deg",
        &[arg("d"), arg("m"), arg("s")], &dbl, runtime::ScalarCallback::new(h),
        Some(&opts("Degrees-minutes-seconds -> decimal degrees")))?;
    Ok(())
}

fn next(f: F) -> u32 {
    let h = NEXT.fetch_add(1, Ordering::Relaxed);
    handlers().lock().unwrap().insert(h, f);
    h
}

#[derive(Clone, Copy, PartialEq)]
enum F { GalL, GalB, Sep, Hms, Dms }
static NEXT: AtomicU32 = AtomicU32::new(1);
static HANDLERS: OnceLock<Mutex<HashMap<u32, F>>> = OnceLock::new();
fn handlers() -> &'static Mutex<HashMap<u32, F>> { HANDLERS.get_or_init(|| Mutex::new(HashMap::new())) }
