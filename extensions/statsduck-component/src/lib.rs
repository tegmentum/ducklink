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
        register_tables()?;
        Ok(types::Loadresult {
            name: "statsduck".into(),
            version: Some(env!("CARGO_PKG_VERSION").into()),
            requires: vec![
                types::Capabilitykind::Scalar,
                types::Capabilitykind::Table,
            ]
            .into(),
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
        handle: u32,
        args: Vec<types::Duckvalue>,
    ) -> Result<types::Resultset, types::Duckerror> {
        let which = handlers()
            .lock()
            .unwrap()
            .get(&handle)
            .copied()
            .ok_or_else(|| types::Duckerror::Internal("unknown table handle".into()))?;
        // Every table fn returns zero rows on NULL/invalid JSON -- never panics.
        let rows = match which {
            F::Lm => lm_rows(&args),
            F::LmSummary => lm_summary_rows(&args),
            F::CorrMatrix => corr_matrix_rows(&args),
            F::TableOne => table_one_rows(&args),
            F::BinEdges => bin_edges_rows(&args),
            _ => {
                return Err(types::Duckerror::Internal(
                    "scalar handle dispatched as table".into(),
                ))
            }
        };
        Ok(rows.into())
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
        ShapiroWilk => match text_arg(args, 0).as_deref().and_then(parse_vec) {
            Some(x) => json_text(stats::shapiro_wilk(&x)),
            None => types::Duckvalue::Null,
        },
        AndersonDarling => match text_arg(args, 0).as_deref().and_then(parse_vec) {
            Some(x) => json_text(stats::anderson_darling(&x)),
            None => types::Duckvalue::Null,
        },
        KsTest1samp => {
            let sample = text_arg(args, 0).as_deref().and_then(parse_vec);
            let dist = text_arg(args, 1).unwrap_or_else(|| "normal".into());
            // params_json is an optional JSON object; default empty -> defaults.
            let params: Value = text_arg(args, 2)
                .and_then(|s| serde_json::from_str(&s).ok())
                .unwrap_or_else(|| Value::Object(Default::default()));
            match sample {
                Some(s) => json_text(stats::ks_test_1samp(&s, &dist, &params)),
                None => types::Duckvalue::Null,
            }
        }
        KendallTest => {
            let x = text_arg(args, 0).as_deref().and_then(parse_vec);
            let y = text_arg(args, 1).as_deref().and_then(parse_vec);
            match (x, y) {
                (Some(x), Some(y)) => json_text(stats::kendall_test(&x, &y)),
                _ => types::Duckvalue::Null,
            }
        }
        PoibinCdf => {
            let probs = text_arg(args, 0).as_deref().and_then(parse_vec);
            let k = f64_arg(args, 1);
            match (probs, k) {
                (Some(p), Some(k)) => fin(stats::poibin_cdf(&p, k as i64).unwrap_or(f64::NAN)),
                _ => types::Duckvalue::Null,
            }
        }
        // Table-function handles never reach scalar eval.
        Lm | LmSummary | CorrMatrix | TableOne | BinEdges => types::Duckvalue::Null,
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
    // Priority 1 -- the "hard" tests (real algorithms + correct p-values).
    ShapiroWilk,
    AndersonDarling,
    KsTest1samp,
    KendallTest,
    PoibinCdf,
    // Priority 2 -- table functions.
    Lm,
    LmSummary,
    CorrMatrix,
    TableOne,
    BinEdges,
}

// OUT OF SCOPE (documented, deliberately not implemented):
//   r* random samplers   -- non-deterministic; violates DETERMINISTIC|STATELESS
//                           and is not reproducible in deterministic smoke tests.
//   read_stat / SAS/SPSS COPY export -- needs file I/O (the `files` capability),
//                           not available to scalar/table functions here.
//   VISUALIZE / ggsql     -- a SQL parser hook, not a registrable function.

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

    // ---- Priority 1: the hard tests (real algorithms, correct p-values) ----
    reg_fn(
        "shapiro_wilk",
        vec![txt("sample")],
        types::Logicaltype::Text,
        F::ShapiroWilk,
        "Shapiro-Wilk normality test (Royston 1992) -> {W,p_value,n}",
    )?;
    reg_fn(
        "anderson_darling",
        vec![txt("sample")],
        types::Logicaltype::Text,
        F::AndersonDarling,
        "Anderson-Darling normality test -> {A_squared,A_star,p_value,n}",
    )?;
    reg_fn(
        "ks_test_1samp",
        vec![txt("sample"), txt("dist"), txt("params")],
        types::Logicaltype::Text,
        F::KsTest1samp,
        "1-sample KS test vs normal/uniform/exponential -> {D,p_value,n}",
    )?;
    reg_fn(
        "kendall_test",
        vec![txt("x"), txt("y")],
        types::Logicaltype::Text,
        F::KendallTest,
        "Kendall's tau-b with tie correction -> {tau,p_value,z_statistic,...}",
    )?;
    reg_fn(
        "poibin_cdf",
        vec![txt("probs"), dbl("k")],
        types::Logicaltype::Float64,
        F::PoibinCdf,
        "Poisson-binomial CDF P(X<=k) over a JSON array of probabilities",
    )?;

    Ok(())
}

// ============================================================================
// Table functions. Data crosses the WIT boundary as a JSON VARCHAR argument
// (LIST args don't cross cleanly). Each builder returns zero rows on NULL or
// invalid JSON, and never panics.
// ============================================================================

type Row = std::vec::Vec<types::Duckvalue>;
type Rows = std::vec::Vec<Row>;

fn txt_v(s: impl ToString) -> types::Duckvalue {
    types::Duckvalue::Text(s.to_string().into())
}
fn dbl_v(v: f64) -> types::Duckvalue {
    if v.is_finite() {
        types::Duckvalue::Float64(v)
    } else {
        types::Duckvalue::Null
    }
}
fn i64_v(v: i64) -> types::Duckvalue {
    types::Duckvalue::Int64(v)
}

/// Parse a column of numbers from a JSON value (array of finite numbers).
fn json_col(v: &Value) -> Option<std::vec::Vec<f64>> {
    let arr = v.as_array()?;
    let out: Option<std::vec::Vec<f64>> = arr.iter().map(|x| x.as_f64()).collect();
    let out = out?;
    if out.iter().all(|x| x.is_finite()) {
        Some(out)
    } else {
        None
    }
}

/// Parse the data_json argument (arg 0) into a serde_json Value object.
fn data_obj(args: &[types::Duckvalue]) -> Option<Value> {
    let s = text_arg(args, 0)?;
    let v: Value = serde_json::from_str(&s).ok()?;
    if v.is_object() {
        Some(v)
    } else {
        None
    }
}

/// Extract y + predictor columns + names from {y:[...], x:[[col],...]} for lm.
/// `x` is a JSON array of predictor columns; optional `names` array labels them.
fn lm_inputs(args: &[types::Duckvalue]) -> Option<(Vec<f64>, Vec<Vec<f64>>, Vec<String>)> {
    let obj = data_obj(args)?;
    let y = json_col(obj.get("y")?)?;
    let xraw = obj.get("x")?.as_array()?;
    let mut x = std::vec::Vec::with_capacity(xraw.len());
    for c in xraw {
        x.push(json_col(c)?);
    }
    // names: optional, from formula_json arg 1 {"names":[...]} or data's "names".
    let names: Vec<String> = text_arg(args, 1)
        .and_then(|s| serde_json::from_str::<Value>(&s).ok())
        .and_then(|v| v.get("names").and_then(|n| n.as_array()).cloned())
        .map(|arr| {
            arr.iter()
                .map(|n| n.as_str().unwrap_or("").to_string())
                .collect()
        })
        .unwrap_or_default();
    Some((y, x, names))
}

fn lm_rows(args: &[types::Duckvalue]) -> Rows {
    let Some((y, x, names)) = lm_inputs(args) else {
        return Rows::new();
    };
    let Some(fit) = stats::ols_fit(&y, &x, &names) else {
        return Rows::new();
    };
    (0..fit.terms.len())
        .map(|i| {
            vec![
                txt_v(&fit.terms[i]),
                dbl_v(fit.estimate[i]),
                dbl_v(fit.std_error[i]),
                dbl_v(fit.t_value[i]),
                dbl_v(fit.p_value[i]),
            ]
        })
        .collect()
}

fn lm_summary_rows(args: &[types::Duckvalue]) -> Rows {
    let Some((y, x, names)) = lm_inputs(args) else {
        return Rows::new();
    };
    let Some(fit) = stats::ols_fit(&y, &x, &names) else {
        return Rows::new();
    };
    [
        ("r_squared", fit.r_squared),
        ("adj_r_squared", fit.adj_r_squared),
        ("f_statistic", fit.f_statistic),
        ("f_pvalue", fit.f_pvalue),
        ("residual_std_error", fit.residual_std_error),
        ("df", fit.df_resid),
    ]
    .iter()
    .map(|(k, v)| vec![txt_v(k), dbl_v(*v)])
    .collect()
}

fn corr_matrix_rows(args: &[types::Duckvalue]) -> Rows {
    let Some(obj) = data_obj(args) else {
        return Rows::new();
    };
    let method = text_arg(args, 1).unwrap_or_else(|| "pearson".into());
    let spearman = method.eq_ignore_ascii_case("spearman");
    // preserve insertion order of the columns.
    let Some(map) = obj.as_object() else {
        return Rows::new();
    };
    let cols: std::vec::Vec<(String, std::vec::Vec<f64>)> = map
        .iter()
        .filter_map(|(k, v)| json_col(v).map(|c| (k.clone(), c)))
        .collect();
    let mut out = Rows::new();
    for i in 0..cols.len() {
        for j in 0..cols.len() {
            let r = if spearman {
                stats::spearman_corr(&cols[i].1, &cols[j].1)
            } else {
                stats::pearson_corr(&cols[i].1, &cols[j].1)
            };
            match r {
                Some(r) => out.push(vec![
                    txt_v(&cols[i].0),
                    txt_v(&cols[j].0),
                    dbl_v(r),
                ]),
                None => {} // skip degenerate / mismatched pairs
            }
        }
    }
    out
}

fn table_one_rows(args: &[types::Duckvalue]) -> Rows {
    let Some(obj) = data_obj(args) else {
        return Rows::new();
    };
    let Some(map) = obj.as_object() else {
        return Rows::new();
    };
    map.iter()
        .filter_map(|(k, v)| {
            let col = json_col(v)?;
            let (n, mean, sd, median, mn, mx) = stats::describe(&col)?;
            Some(vec![
                txt_v(k),
                i64_v(n as i64),
                dbl_v(mean),
                dbl_v(sd),
                dbl_v(median),
                dbl_v(mn),
                dbl_v(mx),
            ])
        })
        .collect()
}

fn bin_edges_rows(args: &[types::Duckvalue]) -> Rows {
    // bin_edges(sample_json VARCHAR, method VARCHAR, bins INTEGER)
    let sample = match text_arg(args, 0).as_deref().and_then(parse_vec) {
        Some(s) => s,
        None => return Rows::new(),
    };
    let method = text_arg(args, 1).unwrap_or_else(|| "equal".into());
    let bins = match args.get(2) {
        Some(types::Duckvalue::Int32(v)) => *v as i64,
        Some(types::Duckvalue::Int64(v)) => *v,
        _ => 10,
    };
    let Some(edges) = stats::bin_edges(&sample, &method, bins) else {
        return Rows::new();
    };
    edges
        .into_iter()
        .map(|b| {
            vec![
                types::Duckvalue::Int32(b.index as i32),
                dbl_v(b.lower),
                dbl_v(b.upper),
                i64_v(b.count),
            ]
        })
        .collect()
}

fn register_tables() -> Result<(), types::Duckerror> {
    let cap = runtime::get_capability(types::Capabilitykind::Table)
        .ok_or_else(|| types::Duckerror::Internal("no table capability".into()))?;
    let reg = match cap {
        runtime::Capability::Table(r) => r,
        _ => return Err(types::Duckerror::Internal("bad table capability".into())),
    };

    use types::Logicaltype::{Float64, Int32, Int64, Text};
    let col = |name: &str, logical: types::Logicaltype| types::Columndef {
        name: name.into(),
        logical,
    };
    let txt = |n: &str| runtime::Funcarg {
        name: Some(n.into()),
        logical: Text,
    };
    let int = |n: &str| runtime::Funcarg {
        name: Some(n.into()),
        logical: Int32,
    };

    let reg_table = |name: &str,
                         a: std::vec::Vec<runtime::Funcarg>,
                         cols: std::vec::Vec<types::Columndef>,
                         f: F,
                         desc: &str|
     -> Result<(), types::Duckerror> {
        let h = NEXT.fetch_add(1, Ordering::Relaxed);
        handlers().lock().unwrap().insert(h, f);
        reg.register(
            name,
            &a,
            &cols,
            runtime::TableCallback::new(h),
            Some(&runtime::Extopts {
                description: Some(desc.into()),
                tags: vec!["stats".into()],
            }),
        )?;
        Ok(())
    };

    reg_table(
        "lm",
        vec![txt("data_json"), txt("formula_json")],
        vec![
            col("term", Text),
            col("estimate", Float64),
            col("std_error", Float64),
            col("t_value", Float64),
            col("p_value", Float64),
        ],
        F::Lm,
        "OLS linear regression coefficient table from {y:[...],x:[[...]]}",
    )?;
    reg_table(
        "lm_summary",
        vec![txt("data_json"), txt("formula_json")],
        vec![col("metric", Text), col("value", Float64)],
        F::LmSummary,
        "OLS fit summary (r_squared, F, residual_std_error, df, ...)",
    )?;
    reg_table(
        "corr_matrix",
        vec![txt("data_json"), txt("method")],
        vec![
            col("var1", Text),
            col("var2", Text),
            col("correlation", Float64),
        ],
        F::CorrMatrix,
        "pairwise pearson/spearman correlations over the JSON columns (long format)",
    )?;
    reg_table(
        "table_one",
        vec![txt("data_json")],
        vec![
            col("variable", Text),
            col("n", Int64),
            col("mean", Float64),
            col("sd", Float64),
            col("median", Float64),
            col("min", Float64),
            col("max", Float64),
        ],
        F::TableOne,
        "descriptive summary per numeric column of the JSON data",
    )?;
    reg_table(
        "bin_edges",
        vec![txt("sample_json"), txt("method"), int("bins")],
        vec![
            col("bin", Int32),
            col("lower", Float64),
            col("upper", Float64),
            col("count", Int64),
        ],
        F::BinEdges,
        "histogram binning (equal-width / sturges / fd) with per-bin counts",
    )?;

    Ok(())
}
