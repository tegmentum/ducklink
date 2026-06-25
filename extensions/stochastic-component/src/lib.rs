//! Statistical probability-distribution scalars (via `statrs`) that DuckDB core
//! lacks. PDF/PMF, CDF and inverse-CDF (quantile) for common distributions:
//!   normal_cdf/normal_pdf/normal_quantile(x, mean, sd),
//!   binomial_pmf(k, n, p), poisson_pmf(k, lambda),
//!   exponential_cdf(x, rate), beta_cdf(x, alpha, beta).
//! NULL input or invalid params (e.g. sd<=0) -> NULL; never panics.
use std::collections::HashMap;
use std::sync::{atomic::{AtomicU32, Ordering}, Mutex, OnceLock};
use wit_bindgen::rt::string::String;
use wit_bindgen::rt::vec::Vec;
wit_bindgen::generate!({ path: "./wit", world: "duckdb:extension/duckdb-extension" });
use duckdb::extension::{runtime, types};
use exports::duckdb::extension::{callback_dispatch, guest};
use statrs::distribution::{Beta, Binomial, ContinuousCDF, Exp, Normal, Poisson};
use statrs::distribution::{Continuous, Discrete};

struct Extension;
impl guest::Guest for Extension {
    fn load() -> Result<types::Loadresult, types::Duckerror> {
        register_scalars()?;
        Ok(types::Loadresult { name: "stochastic".into(), version: Some(env!("CARGO_PKG_VERSION").into()), requires: Vec::new().into() })
    }
    fn reconfigure(_k: Vec<String>) -> Result<bool, types::Duckerror> { Ok(false) }
    fn shutdown() -> Result<bool, types::Duckerror> { Ok(false) }
}

fn f64_arg(args: &[types::Duckvalue], i: usize) -> Option<f64> {
    match args.get(i) {
        Some(types::Duckvalue::Float64(v)) => Some(*v),
        Some(types::Duckvalue::Int64(v)) => Some(*v as f64),
        _ => None,
    }
}
fn i64_arg(args: &[types::Duckvalue], i: usize) -> Option<i64> {
    match args.get(i) {
        Some(types::Duckvalue::Int64(v)) => Some(*v),
        Some(types::Duckvalue::Float64(v)) => Some(*v as i64),
        _ => None,
    }
}
fn fin(v: f64) -> types::Duckvalue {
    if v.is_finite() { types::Duckvalue::Float64(v) } else { types::Duckvalue::Null }
}

impl callback_dispatch::Guest for Extension {
    fn call_scalar_batch(h: u32, rows: Vec<Vec<types::Duckvalue>>, ctx: types::Invokeinfo) -> Result<Vec<types::Duckvalue>, types::Duckerror> {
        let base = ctx.rowindex.unwrap_or(0); let mut out = Vec::with_capacity(rows.len());
        for (i, a) in rows.into_iter().enumerate() {
            out.push(Self::call_scalar(h, a, types::Invokeinfo { rowindex: Some(base + i as u64), iswindow: ctx.iswindow })?);
        }
        Ok(out)
    }
    fn call_scalar(handle: u32, args: Vec<types::Duckvalue>, _c: types::Invokeinfo) -> Result<types::Duckvalue, types::Duckerror> {
        let which = handlers().lock().unwrap().get(&handle).copied()
            .ok_or_else(|| types::Duckerror::Internal("unknown scalar handle".into()))?;
        Ok(match which {
            S::NormalCdf | S::NormalPdf | S::NormalQuantile => {
                match (f64_arg(&args, 0), f64_arg(&args, 1), f64_arg(&args, 2)) {
                    (Some(x), Some(mean), Some(sd)) => match Normal::new(mean, sd) {
                        Ok(d) => match which {
                            S::NormalCdf => fin(d.cdf(x)),
                            S::NormalPdf => fin(d.pdf(x)),
                            // quantile is the inverse CDF; p must be in [0,1].
                            _ => if (0.0..=1.0).contains(&x) { fin(d.inverse_cdf(x)) } else { types::Duckvalue::Null },
                        },
                        Err(_) => types::Duckvalue::Null,
                    },
                    _ => types::Duckvalue::Null,
                }
            }
            S::BinomialPmf => {
                match (i64_arg(&args, 0), i64_arg(&args, 1), f64_arg(&args, 2)) {
                    (Some(k), Some(n), Some(p)) if k >= 0 && n >= 0 => match Binomial::new(p, n as u64) {
                        Ok(d) => fin(d.pmf(k as u64)),
                        Err(_) => types::Duckvalue::Null,
                    },
                    _ => types::Duckvalue::Null,
                }
            }
            S::PoissonPmf => {
                match (i64_arg(&args, 0), f64_arg(&args, 1)) {
                    (Some(k), Some(lambda)) if k >= 0 => match Poisson::new(lambda) {
                        Ok(d) => fin(d.pmf(k as u64)),
                        Err(_) => types::Duckvalue::Null,
                    },
                    _ => types::Duckvalue::Null,
                }
            }
            S::ExponentialCdf => {
                match (f64_arg(&args, 0), f64_arg(&args, 1)) {
                    (Some(x), Some(rate)) => match Exp::new(rate) {
                        Ok(d) => fin(d.cdf(x)),
                        Err(_) => types::Duckvalue::Null,
                    },
                    _ => types::Duckvalue::Null,
                }
            }
            S::BetaCdf => {
                match (f64_arg(&args, 0), f64_arg(&args, 1), f64_arg(&args, 2)) {
                    (Some(x), Some(alpha), Some(beta)) => match Beta::new(alpha, beta) {
                        Ok(d) => fin(d.cdf(x)),
                        Err(_) => types::Duckvalue::Null,
                    },
                    _ => types::Duckvalue::Null,
                }
            }
        })
    }
    fn call_table(_h: u32, _a: Vec<types::Duckvalue>) -> Result<types::Resultset, types::Duckerror> { Err(types::Duckerror::Unsupported("stochastic: no table fns".into())) }
    fn call_aggregate(_h: u32, _r: types::Rowbatch) -> Result<types::Duckvalue, types::Duckerror> { Err(types::Duckerror::Unsupported("stochastic: no aggs".into())) }
    fn call_pragma(_h: u32, _a: Vec<types::Duckvalue>) -> Result<Option<types::Duckvalue>, types::Duckerror> { Err(types::Duckerror::Unsupported("stochastic: no pragmas".into())) }
    fn call_cast(_h: u32, _v: types::Duckvalue) -> Result<types::Duckvalue, types::Duckerror> { Err(types::Duckerror::Unsupported("stochastic: no casts".into())) }
}
export!(Extension);

fn reg3(reg: &runtime::ScalarRegistry, name: &str, a0: &str, a1: &str, a2: &str, s: S, desc: &str, det: types::Funcflags) -> Result<(), types::Duckerror> {
    let h = NEXT.fetch_add(1, Ordering::Relaxed); handlers().lock().unwrap().insert(h, s);
    reg.register(name, &[
        runtime::Funcarg { name: Some(a0.into()), logical: types::Logicaltype::Float64 },
        runtime::Funcarg { name: Some(a1.into()), logical: types::Logicaltype::Float64 },
        runtime::Funcarg { name: Some(a2.into()), logical: types::Logicaltype::Float64 }],
        &types::Logicaltype::Float64, runtime::ScalarCallback::new(h),
        Some(&runtime::Funcopts { description: Some(desc.into()), tags: vec!["stats".into()], attributes: det }))?;
    Ok(())
}

fn register_scalars() -> Result<(), types::Duckerror> {
    let cap = runtime::get_capability(types::Capabilitykind::Scalar).ok_or_else(|| types::Duckerror::Internal("no scalar capability".into()))?;
    let reg = match cap { runtime::Capability::Scalar(r) => r, _ => return Err(types::Duckerror::Internal("bad capability".into())) };
    let det = types::Funcflags::DETERMINISTIC | types::Funcflags::STATELESS;

    reg3(&reg, "normal_cdf", "x", "mean", "sd", S::NormalCdf, "normal distribution CDF P(X<=x)", det)?;
    reg3(&reg, "normal_pdf", "x", "mean", "sd", S::NormalPdf, "normal distribution PDF density at x", det)?;
    reg3(&reg, "normal_quantile", "p", "mean", "sd", S::NormalQuantile, "normal distribution inverse CDF (quantile)", det)?;
    reg3(&reg, "beta_cdf", "x", "alpha", "beta", S::BetaCdf, "beta distribution CDF P(X<=x)", det)?;

    // binomial_pmf(k BIGINT, n BIGINT, p DOUBLE)
    let h = NEXT.fetch_add(1, Ordering::Relaxed); handlers().lock().unwrap().insert(h, S::BinomialPmf);
    reg.register("binomial_pmf", &[
        runtime::Funcarg { name: Some("k".into()), logical: types::Logicaltype::Int64 },
        runtime::Funcarg { name: Some("n".into()), logical: types::Logicaltype::Int64 },
        runtime::Funcarg { name: Some("p".into()), logical: types::Logicaltype::Float64 }],
        &types::Logicaltype::Float64, runtime::ScalarCallback::new(h),
        Some(&runtime::Funcopts { description: Some("binomial distribution PMF P(X=k)".into()), tags: vec!["stats".into()], attributes: det }))?;

    // poisson_pmf(k BIGINT, lambda DOUBLE)
    let h = NEXT.fetch_add(1, Ordering::Relaxed); handlers().lock().unwrap().insert(h, S::PoissonPmf);
    reg.register("poisson_pmf", &[
        runtime::Funcarg { name: Some("k".into()), logical: types::Logicaltype::Int64 },
        runtime::Funcarg { name: Some("lambda".into()), logical: types::Logicaltype::Float64 }],
        &types::Logicaltype::Float64, runtime::ScalarCallback::new(h),
        Some(&runtime::Funcopts { description: Some("poisson distribution PMF P(X=k)".into()), tags: vec!["stats".into()], attributes: det }))?;

    // exponential_cdf(x DOUBLE, rate DOUBLE)
    let h = NEXT.fetch_add(1, Ordering::Relaxed); handlers().lock().unwrap().insert(h, S::ExponentialCdf);
    reg.register("exponential_cdf", &[
        runtime::Funcarg { name: Some("x".into()), logical: types::Logicaltype::Float64 },
        runtime::Funcarg { name: Some("rate".into()), logical: types::Logicaltype::Float64 }],
        &types::Logicaltype::Float64, runtime::ScalarCallback::new(h),
        Some(&runtime::Funcopts { description: Some("exponential distribution CDF P(X<=x)".into()), tags: vec!["stats".into()], attributes: det }))?;

    Ok(())
}

#[derive(Clone, Copy, PartialEq)]
enum S { NormalCdf, NormalPdf, NormalQuantile, BinomialPmf, PoissonPmf, ExponentialCdf, BetaCdf }
static NEXT: AtomicU32 = AtomicU32::new(1);
static HANDLERS: OnceLock<Mutex<HashMap<u32, S>>> = OnceLock::new();
fn handlers() -> &'static Mutex<HashMap<u32, S>> { HANDLERS.get_or_init(|| Mutex::new(HashMap::new())) }
