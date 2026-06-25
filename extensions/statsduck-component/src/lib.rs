//! Classical statistics for DuckDB, componentized from `the-stats-duck`.
//!
//! The WIT scalar surface carries only scalar values (no LIST/ARRAY), so the
//! tests take their sample(s) as JSON number arrays in VARCHAR and return a
//! JSON object VARCHAR holding {statistic, p_value, ...}:
//!
//!   ttest_1samp(sample VARCHAR, mu DOUBLE, alt VARCHAR)        -> VARCHAR
//!   ttest_2samp(a VARCHAR, b VARCHAR, equal_var BOOL, alt)     -> VARCHAR
//!   ttest_paired(a VARCHAR, b VARCHAR, alt VARCHAR)            -> VARCHAR
//!   mann_whitney_u(a, b, alt, continuity BOOL)                 -> VARCHAR
//!   wilcoxon_signed_rank(a, b, alt, continuity BOOL)           -> VARCHAR
//!   sign_test_1samp(sample, mu, alt) / sign_test_paired(a,b,alt)
//!   pearson_test(x, y, alt) / spearman_test(x, y, alt)         -> VARCHAR
//!   anova_oneway(groups VARCHAR)        (JSON array of arrays) -> VARCHAR
//!   chisq_goodness_of_fit(observed, expected)                 -> VARCHAR
//!   chisq_independence(table VARCHAR, continuity BOOL)        -> VARCHAR
//!   jarque_bera(sample) / ks_test_2samp(a, b)                  -> VARCHAR
//!   adjust_p(pvals VARCHAR, method VARCHAR)                    -> VARCHAR (JSON array)
//!
//! Plus the test-supporting distribution CDF scalars that the lean core and the
//! `stochastic` component do NOT already provide (stochastic ships normal /
//! binomial / poisson / exponential / beta only):
//!   t_cdf(x, df) / chisq_cdf(x, df) / f_cdf(x, df1, df2)       -> DOUBLE
//!   gamma_cdf(x, shape, rate) / weibull_cdf(x, shape, scale)   -> DOUBLE
//!   lognormal_cdf(x, meanlog, sdlog)                           -> DOUBLE
//!
//! Everything is NULL-safe: NULL in, malformed JSON, length mismatch, too few
//! observations, or a degenerate parameter -> NULL. Nothing panics.
//! All functions are DETERMINISTIC | STATELESS.

mod stats;

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

use serde_json::Value;
use statrs::distribution::{
    ChiSquared, ContinuousCDF, FisherSnedecor, Gamma, LogNormal, StudentsT, Weibull,
};

struct Extension;

impl guest::Guest for Extension {
    fn load() -> Result<types::Loadresult, types::Duckerror> {
        register_scalars()?;
        Ok(types::Loadresult {
            name: "statsduck".into(),
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

// ---- argument helpers -------------------------------------------------------

fn text_arg(args: &[types::Duckvalue], i: usize) -> Option<std::string::String> {
    match args.get(i) {
        Some(types::Duckvalue::Text(s)) => Some(s.to_string()),
        _ => None,
    }
}
fn f64_arg(args: &[types::Duckvalue], i: usize) -> Option<f64> {
    match args.get(i) {
        Some(types::Duckvalue::Float64(v)) => Some(*v),
        Some(types::Duckvalue::Int64(v)) => Some(*v as f64),
        _ => None,
    }
}
fn bool_arg(args: &[types::Duckvalue], i: usize) -> bool {
    matches!(args.get(i), Some(types::Duckvalue::Boolean(true)))
}
fn alt_arg(args: &[types::Duckvalue], i: usize) -> std::string::String {
    match args.get(i) {
        Some(types::Duckvalue::Text(s)) => s.to_string(),
        _ => "two-sided".to_string(),
    }
}

/// Parse a JSON array of finite numbers, e.g. "[1.0, 2, 3.5]".
fn parse_vec(s: &str) -> Option<std::vec::Vec<f64>> {
    let v: Value = serde_json::from_str(s).ok()?;
    let arr = v.as_array()?;
    let out: Option<std::vec::Vec<f64>> = arr.iter().map(|x| x.as_f64()).collect();
    let out = out?;
    if out.iter().all(|x| x.is_finite()) {
        Some(out)
    } else {
        None
    }
}

/// Parse a JSON array of arrays of numbers (groups / contingency table).
fn parse_matrix(s: &str) -> Option<std::vec::Vec<std::vec::Vec<f64>>> {
    let v: Value = serde_json::from_str(s).ok()?;
    let arr = v.as_array()?;
    let mut out = std::vec::Vec::with_capacity(arr.len());
    for row in arr {
        let r = row.as_array()?;
        let nums: Option<std::vec::Vec<f64>> = r.iter().map(|x| x.as_f64()).collect();
        let nums = nums?;
        if !nums.iter().all(|x| x.is_finite()) {
            return None;
        }
        out.push(nums);
    }
    Some(out)
}

/// Render a serde_json::Value (test result object) as a DuckDB Text value.
fn json_text(v: Option<Value>) -> types::Duckvalue {
    match v.and_then(|val| serde_json::to_string(&val).ok()) {
        Some(s) => types::Duckvalue::Text(s.into()),
        None => types::Duckvalue::Null,
    }
}

fn fin(v: f64) -> types::Duckvalue {
    if v.is_finite() {
        types::Duckvalue::Float64(v)
    } else {
        types::Duckvalue::Null
    }
}

// ---- dispatch ---------------------------------------------------------------

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
        Ok(eval(which, &args))
    }

    fn call_table(
        _h: u32,
        _a: Vec<types::Duckvalue>,
    ) -> Result<types::Resultset, types::Duckerror> {
        Err(types::Duckerror::Unsupported(
            "statsduck: no table fns".into(),
        ))
    }
    fn call_aggregate(
        _h: u32,
        _r: types::Rowbatch,
    ) -> Result<types::Duckvalue, types::Duckerror> {
        Err(types::Duckerror::Unsupported("statsduck: no aggs".into()))
    }
    fn call_pragma(
        _h: u32,
        _a: Vec<types::Duckvalue>,
    ) -> Result<Option<types::Duckvalue>, types::Duckerror> {
        Err(types::Duckerror::Unsupported("statsduck: no pragmas".into()))
    }
    fn call_cast(
        _h: u32,
        _v: types::Duckvalue,
    ) -> Result<types::Duckvalue, types::Duckerror> {
        Err(types::Duckerror::Unsupported("statsduck: no casts".into()))
    }
}
export!(Extension);

/// The actual per-function evaluation. Anything invalid -> Null.
fn eval(which: F, args: &[types::Duckvalue]) -> types::Duckvalue {
    use F::*;
    match which {
        Ttest1samp => {
            match (text_arg(args, 0).as_deref().and_then(parse_vec), f64_arg(args, 1)) {
                (Some(x), Some(mu)) => json_text(stats::ttest_1samp(&x, mu, &alt_arg(args, 2))),
                _ => types::Duckvalue::Null,
            }
        }
        Ttest2samp => {
            let a = text_arg(args, 0).as_deref().and_then(parse_vec);
            let b = text_arg(args, 1).as_deref().and_then(parse_vec);
            match (a, b) {
                (Some(a), Some(b)) => json_text(stats::ttest_2samp(
                    &a,
                    &b,
                    bool_arg(args, 2),
                    &alt_arg(args, 3),
                )),
                _ => types::Duckvalue::Null,
            }
        }
        TtestPaired => {
            let a = text_arg(args, 0).as_deref().and_then(parse_vec);
            let b = text_arg(args, 1).as_deref().and_then(parse_vec);
            match (a, b) {
                (Some(a), Some(b)) => json_text(stats::ttest_paired(&a, &b, &alt_arg(args, 2))),
                _ => types::Duckvalue::Null,
            }
        }
        MannWhitney => {
            let a = text_arg(args, 0).as_deref().and_then(parse_vec);
            let b = text_arg(args, 1).as_deref().and_then(parse_vec);
            match (a, b) {
                (Some(a), Some(b)) => json_text(stats::mann_whitney_u(
                    &a,
                    &b,
                    &alt_arg(args, 2),
                    bool_arg(args, 3),
                )),
                _ => types::Duckvalue::Null,
            }
        }
        Wilcoxon => {
            let a = text_arg(args, 0).as_deref().and_then(parse_vec);
            let b = text_arg(args, 1).as_deref().and_then(parse_vec);
            match (a, b) {
                (Some(a), Some(b)) => json_text(stats::wilcoxon_signed_rank(
                    &a,
                    &b,
                    &alt_arg(args, 2),
                    bool_arg(args, 3),
                )),
                _ => types::Duckvalue::Null,
            }
        }
        Sign1samp => {
            match (text_arg(args, 0).as_deref().and_then(parse_vec), f64_arg(args, 1)) {
                (Some(x), Some(mu)) => json_text(stats::sign_test_1samp(&x, mu, &alt_arg(args, 2))),
                _ => types::Duckvalue::Null,
            }
        }
        SignPaired => {
            let a = text_arg(args, 0).as_deref().and_then(parse_vec);
            let b = text_arg(args, 1).as_deref().and_then(parse_vec);
            match (a, b) {
                (Some(a), Some(b)) => json_text(stats::sign_test_paired(&a, &b, &alt_arg(args, 2))),
                _ => types::Duckvalue::Null,
            }
        }
        Pearson => {
            let x = text_arg(args, 0).as_deref().and_then(parse_vec);
            let y = text_arg(args, 1).as_deref().and_then(parse_vec);
            match (x, y) {
                (Some(x), Some(y)) => json_text(stats::pearson_test(&x, &y, &alt_arg(args, 2))),
                _ => types::Duckvalue::Null,
            }
        }
        Spearman => {
            let x = text_arg(args, 0).as_deref().and_then(parse_vec);
            let y = text_arg(args, 1).as_deref().and_then(parse_vec);
            match (x, y) {
                (Some(x), Some(y)) => json_text(stats::spearman_test(&x, &y, &alt_arg(args, 2))),
                _ => types::Duckvalue::Null,
            }
        }
        Anova => match text_arg(args, 0).as_deref().and_then(parse_matrix) {
            Some(g) => json_text(stats::anova_oneway(&g)),
            None => types::Duckvalue::Null,
        },
        ChisqGof => {
            let obs = text_arg(args, 0).as_deref().and_then(parse_vec);
            let exp = text_arg(args, 1).as_deref().and_then(parse_vec);
            match obs {
                Some(o) => json_text(stats::chisq_goodness_of_fit(&o, exp.as_deref())),
                None => types::Duckvalue::Null,
            }
        }
        ChisqIndep => match text_arg(args, 0).as_deref().and_then(parse_matrix) {
            Some(t) => json_text(stats::chisq_independence(&t, bool_arg(args, 1))),
            None => types::Duckvalue::Null,
        },
        JarqueBera => match text_arg(args, 0).as_deref().and_then(parse_vec) {
            Some(x) => json_text(stats::jarque_bera(&x)),
            None => types::Duckvalue::Null,
        },
        Ks2samp => {
            let a = text_arg(args, 0).as_deref().and_then(parse_vec);
            let b = text_arg(args, 1).as_deref().and_then(parse_vec);
            match (a, b) {
                (Some(a), Some(b)) => json_text(stats::ks_test_2samp(&a, &b)),
                _ => types::Duckvalue::Null,
            }
        }
        AdjustP => {
            let p = text_arg(args, 0).as_deref().and_then(parse_vec);
            let method = text_arg(args, 1).unwrap_or_else(|| "none".into());
            match p.and_then(|p| stats::adjust_p(&p, &method)) {
                Some(adj) => json_text(Some(Value::from(adj))),
                None => types::Duckvalue::Null,
            }
        }
        TCdf => match (f64_arg(args, 0), f64_arg(args, 1)) {
            (Some(x), Some(df)) if df > 0.0 => match StudentsT::new(0.0, 1.0, df) {
                Ok(d) => fin(d.cdf(x)),
                Err(_) => types::Duckvalue::Null,
            },
            _ => types::Duckvalue::Null,
        },
        ChisqCdf => match (f64_arg(args, 0), f64_arg(args, 1)) {
            (Some(x), Some(df)) if df > 0.0 => match ChiSquared::new(df) {
                Ok(d) => fin(d.cdf(x)),
                Err(_) => types::Duckvalue::Null,
            },
            _ => types::Duckvalue::Null,
        },
        FCdf => match (f64_arg(args, 0), f64_arg(args, 1), f64_arg(args, 2)) {
            (Some(x), Some(d1), Some(d2)) if d1 > 0.0 && d2 > 0.0 => {
                match FisherSnedecor::new(d1, d2) {
                    Ok(d) => fin(d.cdf(x)),
                    Err(_) => types::Duckvalue::Null,
                }
            }
            _ => types::Duckvalue::Null,
        },
        GammaCdf => match (f64_arg(args, 0), f64_arg(args, 1), f64_arg(args, 2)) {
            (Some(x), Some(shape), Some(rate)) => match Gamma::new(shape, rate) {
                Ok(d) => fin(d.cdf(x)),
                Err(_) => types::Duckvalue::Null,
            },
            _ => types::Duckvalue::Null,
        },
        WeibullCdf => match (f64_arg(args, 0), f64_arg(args, 1), f64_arg(args, 2)) {
            (Some(x), Some(shape), Some(scale)) => match Weibull::new(shape, scale) {
                Ok(d) => fin(d.cdf(x)),
                Err(_) => types::Duckvalue::Null,
            },
            _ => types::Duckvalue::Null,
        },
        LognormalCdf => match (f64_arg(args, 0), f64_arg(args, 1), f64_arg(args, 2)) {
            (Some(x), Some(meanlog), Some(sdlog)) => match LogNormal::new(meanlog, sdlog) {
                Ok(d) => fin(d.cdf(x)),
                Err(_) => types::Duckvalue::Null,
            },
            _ => types::Duckvalue::Null,
        },
    }
}

// ---- registration -----------------------------------------------------------

#[derive(Clone, Copy, PartialEq)]
enum F {
    Ttest1samp,
    Ttest2samp,
    TtestPaired,
    MannWhitney,
    Wilcoxon,
    Sign1samp,
    SignPaired,
    Pearson,
    Spearman,
    Anova,
    ChisqGof,
    ChisqIndep,
    JarqueBera,
    Ks2samp,
    AdjustP,
    TCdf,
    ChisqCdf,
    FCdf,
    GammaCdf,
    WeibullCdf,
    LognormalCdf,
}

static NEXT: AtomicU32 = AtomicU32::new(1);
static HANDLERS: OnceLock<Mutex<HashMap<u32, F>>> = OnceLock::new();
fn handlers() -> &'static Mutex<HashMap<u32, F>> {
    HANDLERS.get_or_init(|| Mutex::new(HashMap::new()))
}

fn register_scalars() -> Result<(), types::Duckerror> {
    let cap = runtime::get_capability(types::Capabilitykind::Scalar)
        .ok_or_else(|| types::Duckerror::Internal("no scalar capability".into()))?;
    let reg = match cap {
        runtime::Capability::Scalar(r) => r,
        _ => return Err(types::Duckerror::Internal("bad capability".into())),
    };
    let det = types::Funcflags::DETERMINISTIC | types::Funcflags::STATELESS;

    let txt = |n: &str| runtime::Funcarg {
        name: Some(n.into()),
        logical: types::Logicaltype::Text,
    };
    let dbl = |n: &str| runtime::Funcarg {
        name: Some(n.into()),
        logical: types::Logicaltype::Float64,
    };
    let boolean = |n: &str| runtime::Funcarg {
        name: Some(n.into()),
        logical: types::Logicaltype::Boolean,
    };

    let reg_fn = |name: &str,
                      a: std::vec::Vec<runtime::Funcarg>,
                      ret: types::Logicaltype,
                      f: F,
                      desc: &str|
     -> Result<(), types::Duckerror> {
        let h = NEXT.fetch_add(1, Ordering::Relaxed);
        handlers().lock().unwrap().insert(h, f);
        reg.register(
            name,
            &a,
            &ret,
            runtime::ScalarCallback::new(h),
            Some(&runtime::Funcopts {
                description: Some(desc.into()),
                tags: vec!["stats".into()],
                attributes: det,
            }),
        )?;
        Ok(())
    };

    // ---- hypothesis tests (JSON-array in, JSON-object out) ----
    reg_fn(
        "ttest_1samp",
        vec![txt("sample"), dbl("mu"), txt("alternative")],
        types::Logicaltype::Text,
        F::Ttest1samp,
        "one-sample t-test on a JSON sample vs mu -> {t_statistic,df,p_value,...}",
    )?;
    reg_fn(
        "ttest_2samp",
        vec![txt("a"), txt("b"), boolean("equal_var"), txt("alternative")],
        types::Logicaltype::Text,
        F::Ttest2samp,
        "two-sample t-test (Welch unless equal_var) -> JSON {t_statistic,df,p_value}",
    )?;
    reg_fn(
        "ttest_paired",
        vec![txt("a"), txt("b"), txt("alternative")],
        types::Logicaltype::Text,
        F::TtestPaired,
        "paired t-test on two equal-length JSON samples -> JSON result",
    )?;
    reg_fn(
        "mann_whitney_u",
        vec![txt("a"), txt("b"), txt("alternative"), boolean("continuity")],
        types::Logicaltype::Text,
        F::MannWhitney,
        "Mann-Whitney U (normal approx) -> {u_statistic,z_statistic,p_value}",
    )?;
    reg_fn(
        "wilcoxon_signed_rank",
        vec![txt("a"), txt("b"), txt("alternative"), boolean("continuity")],
        types::Logicaltype::Text,
        F::Wilcoxon,
        "Wilcoxon signed-rank test (normal approx) -> JSON result",
    )?;
    reg_fn(
        "sign_test_1samp",
        vec![txt("sample"), dbl("mu"), txt("alternative")],
        types::Logicaltype::Text,
        F::Sign1samp,
        "exact binomial sign test vs mu -> {m_statistic,n_pos,n_neg,n_zero,p_value}",
    )?;
    reg_fn(
        "sign_test_paired",
        vec![txt("a"), txt("b"), txt("alternative")],
        types::Logicaltype::Text,
        F::SignPaired,
        "exact paired sign test on differences -> JSON result",
    )?;
    reg_fn(
        "pearson_test",
        vec![txt("x"), txt("y"), txt("alternative")],
        types::Logicaltype::Text,
        F::Pearson,
        "Pearson correlation significance test -> {r,t_statistic,df,p_value,n}",
    )?;
    reg_fn(
        "spearman_test",
        vec![txt("x"), txt("y"), txt("alternative")],
        types::Logicaltype::Text,
        F::Spearman,
        "Spearman rank-correlation test -> {rho,t_statistic,df,p_value,n}",
    )?;
    reg_fn(
        "anova_oneway",
        vec![txt("groups")],
        types::Logicaltype::Text,
        F::Anova,
        "one-way ANOVA over a JSON array of group arrays -> {f_statistic,p_value,...}",
    )?;
    reg_fn(
        "chisq_goodness_of_fit",
        vec![txt("observed"), txt("expected")],
        types::Logicaltype::Text,
        F::ChisqGof,
        "chi-square goodness-of-fit (expected NULL/'' -> uniform) -> JSON result",
    )?;
    reg_fn(
        "chisq_independence",
        vec![txt("table"), boolean("continuity")],
        types::Logicaltype::Text,
        F::ChisqIndep,
        "chi-square test of independence on a JSON contingency table -> JSON result",
    )?;
    reg_fn(
        "jarque_bera",
        vec![txt("sample")],
        types::Logicaltype::Text,
        F::JarqueBera,
        "Jarque-Bera normality test -> {jb_statistic,skewness,excess_kurtosis,p_value}",
    )?;
    reg_fn(
        "ks_test_2samp",
        vec![txt("a"), txt("b")],
        types::Logicaltype::Text,
        F::Ks2samp,
        "two-sample Kolmogorov-Smirnov test -> {d_statistic,p_value,n_x,n_y}",
    )?;
    reg_fn(
        "adjust_p",
        vec![txt("pvals"), txt("method")],
        types::Logicaltype::Text,
        F::AdjustP,
        "multiple-testing p-value correction (bonferroni/holm/hochberg/BH/BY) -> JSON array",
    )?;

    // ---- test-supporting distribution CDFs (new vs stochastic) ----
    reg_fn(
        "t_cdf",
        vec![dbl("x"), dbl("df")],
        types::Logicaltype::Float64,
        F::TCdf,
        "Student's t distribution CDF P(X<=x)",
    )?;
    reg_fn(
        "chisq_cdf",
        vec![dbl("x"), dbl("df")],
        types::Logicaltype::Float64,
        F::ChisqCdf,
        "chi-square distribution CDF P(X<=x)",
    )?;
    reg_fn(
        "f_cdf",
        vec![dbl("x"), dbl("df1"), dbl("df2")],
        types::Logicaltype::Float64,
        F::FCdf,
        "F (Fisher-Snedecor) distribution CDF P(X<=x)",
    )?;
    reg_fn(
        "gamma_cdf",
        vec![dbl("x"), dbl("shape"), dbl("rate")],
        types::Logicaltype::Float64,
        F::GammaCdf,
        "gamma distribution CDF P(X<=x) (shape, rate parameterization)",
    )?;
    reg_fn(
        "weibull_cdf",
        vec![dbl("x"), dbl("shape"), dbl("scale")],
        types::Logicaltype::Float64,
        F::WeibullCdf,
        "Weibull distribution CDF P(X<=x)",
    )?;
    reg_fn(
        "lognormal_cdf",
        vec![dbl("x"), dbl("meanlog"), dbl("sdlog")],
        types::Logicaltype::Float64,
        F::LognormalCdf,
        "log-normal distribution CDF P(X<=x)",
    )?;

    Ok(())
}
