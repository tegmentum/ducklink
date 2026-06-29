//! Generate TPC-DS benchmark reference data as DuckDB table functions.
//!
//!   tpcds_income_band() -> table(
//!       ib_income_band_sk BIGINT,   -- 1..20
//!       ib_lower_bound    BIGINT,   -- inclusive lower income bound
//!       ib_upper_bound    BIGINT)   -- inclusive upper income bound
//!
//!   tpcds_date_dim_sample() -> table(
//!       d_date_sk BIGINT,   -- surrogate key = Julian Day Number
//!       d_date    VARCHAR,  -- ISO calendar date (YYYY-MM-DD)
//!       d_year    BIGINT,
//!       d_moy     BIGINT,   -- month of year (1..12)
//!       d_dom     BIGINT)   -- day of month (1..31)
//!
//! The income bands are the 20 FIXED TPC-DS bands (band 1 = 0..10000, band k =
//! (k-1)*10000+1 .. k*10000). The date sample is a deterministic 366-row slice
//! of date_dim for the leap year 2000: d_date_sk runs 2451545..2451910 (the
//! Julian Day Numbers of 2000-01-01..2000-12-31), exactly matching dsdgen.
//!
//! These are hand-rolled fixed/arithmetic dimension tables. Full dsdgen parity
//! (the heavy C generator's 24 tables and the 99 queries) is out of scope; no
//! pure-Rust TPC-DS generator builds standalone for wasip2.
//!
//! Both functions take no arguments and are fully deterministic. An unknown
//! handle yields zero rows rather than a panic.
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
        register_tables()?;
        Ok(types::Loadresult {
            name: "tpcdsgen".into(),
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

impl callback_dispatch::Guest for Extension {
    // major-4 columnar dispatch: tpcdsgen is table-only, so the three columnar
    // hot methods are Unsupported stubs.
    datalink_extcore::columnar_stub!();

    fn call_scalar(
        _h: u32,
        _a: Vec<types::Duckvalue>,
        _c: types::Invokeinfo,
    ) -> Result<types::Duckvalue, types::Duckerror> {
        Err(types::Duckerror::Unsupported("tpcdsgen: no scalar fns".into()))
    }

    fn call_table(
        handle: u32,
        _args: Vec<types::Duckvalue>,
    ) -> Result<types::Resultset, types::Duckerror> {
        // Unknown handle -> zero rows, never a panic.
        let which = match handlers().lock().unwrap().get(&handle).copied() {
            Some(t) => t,
            None => return Ok(Vec::new().into()),
        };
        let rows = match which {
            T::IncomeBand => income_band(),
            T::DateDimSample => date_dim_sample(),
        };
        Ok(rows.into())
    }

    fn call_pragma(
        _h: u32,
        _a: Vec<types::Duckvalue>,
    ) -> Result<Option<types::Duckvalue>, types::Duckerror> {
        Err(types::Duckerror::Unsupported("tpcdsgen: no pragmas".into()))
    }
    fn call_cast(
        _h: u32,
        _v: types::Duckvalue,
    ) -> Result<types::Duckvalue, types::Duckerror> {
        Err(types::Duckerror::Unsupported("tpcdsgen: no casts".into()))
    }
}

export!(Extension);

/// The 20 FIXED TPC-DS income bands.
///   band 1: lower = 0,            upper = 10000
///   band k: lower = (k-1)*10000+1, upper = k*10000   (k = 2..20)
fn income_band() -> std::vec::Vec<std::vec::Vec<types::Duckvalue>> {
    let mut out = std::vec::Vec::with_capacity(20);
    for k in 1..=20i64 {
        let lower = if k == 1 { 0 } else { (k - 1) * 10_000 + 1 };
        let upper = k * 10_000;
        out.push(vec![
            types::Duckvalue::Int64(k),
            types::Duckvalue::Int64(lower),
            types::Duckvalue::Int64(upper),
        ]);
    }
    out
}

/// A deterministic 366-row sample of date_dim covering the leap year 2000.
/// d_date_sk is the Julian Day Number; 2000-01-01 has JDN 2451545.
fn date_dim_sample() -> std::vec::Vec<std::vec::Vec<types::Duckvalue>> {
    // Julian Day Number of 2000-01-01.
    const JDN_2000_01_01: i64 = 2_451_545;
    let mut out = std::vec::Vec::with_capacity(366);
    for offset in 0..366i64 {
        let sk = JDN_2000_01_01 + offset;
        let (year, moy, dom) = civil_from_jdn(sk);
        let date = format!("{year:04}-{moy:02}-{dom:02}");
        out.push(vec![
            types::Duckvalue::Int64(sk),
            types::Duckvalue::Text(date.into()),
            types::Duckvalue::Int64(year),
            types::Duckvalue::Int64(moy),
            types::Duckvalue::Int64(dom),
        ]);
    }
    out
}

/// Convert a Julian Day Number to a proleptic-Gregorian (year, month, day).
/// Standard Fliegel-Van Flandern algorithm; exact integer arithmetic.
fn civil_from_jdn(jdn: i64) -> (i64, i64, i64) {
    let l = jdn + 68_569;
    let n = (4 * l) / 146_097;
    let l = l - (146_097 * n + 3) / 4;
    let i = (4_000 * (l + 1)) / 1_461_001;
    let l = l - (1_461 * i) / 4 + 31;
    let j = (80 * l) / 2_447;
    let day = l - (2_447 * j) / 80;
    let l = j / 11;
    let month = j + 2 - 12 * l;
    let year = 100 * (n - 49) + i + l;
    (year, month, day)
}

fn register_tables() -> Result<(), types::Duckerror> {
    let cap = runtime::get_capability(types::Capabilitykind::Table)
        .ok_or_else(|| types::Duckerror::Internal("no table capability".into()))?;
    let reg = match cap {
        runtime::Capability::Table(r) => r,
        _ => return Err(types::Duckerror::Internal("bad capability".into())),
    };

    register_one(
        &reg,
        "tpcds_income_band",
        T::IncomeBand,
        vec![
            col("ib_income_band_sk", types::Logicaltype::Int64),
            col("ib_lower_bound", types::Logicaltype::Int64),
            col("ib_upper_bound", types::Logicaltype::Int64),
        ],
        "The 20 fixed TPC-DS income bands (ib_income_band_sk, ib_lower_bound, ib_upper_bound)",
    )?;

    register_one(
        &reg,
        "tpcds_date_dim_sample",
        T::DateDimSample,
        vec![
            col("d_date_sk", types::Logicaltype::Int64),
            col("d_date", types::Logicaltype::Text),
            col("d_year", types::Logicaltype::Int64),
            col("d_moy", types::Logicaltype::Int64),
            col("d_dom", types::Logicaltype::Int64),
        ],
        "A deterministic 366-row sample of TPC-DS date_dim (leap year 2000); d_date_sk is the Julian Day Number",
    )?;

    Ok(())
}

fn col(name: &str, logical: types::Logicaltype) -> types::Columndef {
    types::Columndef {
        name: name.into(),
        logical,
    }
}

fn register_one(
    reg: &runtime::TableRegistry,
    name: &str,
    t: T,
    columns: std::vec::Vec<types::Columndef>,
    description: &str,
) -> Result<(), types::Duckerror> {
    let h = NEXT.fetch_add(1, Ordering::Relaxed);
    handlers().lock().unwrap().insert(h, t);
    let opts = runtime::Extopts {
        description: Some(description.into()),
        tags: vec!["tpcds".into(), "benchmark".into()],
    };
    reg.register(
        name,
        &[],
        &columns,
        runtime::TableCallback::new(h),
        Some(&opts),
    )?;
    Ok(())
}

#[derive(Clone, Copy)]
enum T {
    IncomeBand,
    DateDimSample,
}
static NEXT: AtomicU32 = AtomicU32::new(1);
static HANDLERS: OnceLock<Mutex<HashMap<u32, T>>> = OnceLock::new();
fn handlers() -> &'static Mutex<HashMap<u32, T>> {
    HANDLERS.get_or_init(|| Mutex::new(HashMap::new()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn income_band_has_20_rows() {
        let rows = income_band();
        assert_eq!(rows.len(), 20);
    }

    #[test]
    fn income_band_first_and_last() {
        let rows = income_band();
        // Band 1: 0..10000.
        assert!(matches!(rows[0][0], types::Duckvalue::Int64(1)));
        assert!(matches!(rows[0][1], types::Duckvalue::Int64(0)));
        assert!(matches!(rows[0][2], types::Duckvalue::Int64(10_000)));
        // Band 2: 10001..20000.
        assert!(matches!(rows[1][1], types::Duckvalue::Int64(10_001)));
        assert!(matches!(rows[1][2], types::Duckvalue::Int64(20_000)));
        // Band 20: 190001..200000.
        assert!(matches!(rows[19][0], types::Duckvalue::Int64(20)));
        assert!(matches!(rows[19][1], types::Duckvalue::Int64(190_001)));
        assert!(matches!(rows[19][2], types::Duckvalue::Int64(200_000)));
    }

    #[test]
    fn date_sample_has_366_rows() {
        let rows = date_dim_sample();
        assert_eq!(rows.len(), 366);
    }

    #[test]
    fn date_sample_endpoints() {
        let rows = date_dim_sample();
        // First row: 2000-01-01, JDN 2451545.
        assert!(matches!(rows[0][0], types::Duckvalue::Int64(2_451_545)));
        match &rows[0][1] {
            types::Duckvalue::Text(s) => assert_eq!(s.as_str(), "2000-01-01"),
            _ => panic!("expected text date"),
        }
        assert!(matches!(rows[0][2], types::Duckvalue::Int64(2000)));
        assert!(matches!(rows[0][3], types::Duckvalue::Int64(1)));
        assert!(matches!(rows[0][4], types::Duckvalue::Int64(1)));
        // Last row: 2000-12-31, JDN 2451910.
        assert!(matches!(rows[365][0], types::Duckvalue::Int64(2_451_910)));
        match &rows[365][1] {
            types::Duckvalue::Text(s) => assert_eq!(s.as_str(), "2000-12-31"),
            _ => panic!("expected text date"),
        }
        assert!(matches!(rows[365][3], types::Duckvalue::Int64(12)));
        assert!(matches!(rows[365][4], types::Duckvalue::Int64(31)));
    }

    #[test]
    fn civil_from_jdn_known_dates() {
        // 2000-02-29 exists (leap year): JDN 2451545 + 59 = 2451604.
        assert_eq!(civil_from_jdn(2_451_604), (2000, 2, 29));
        // 2000-03-01: JDN 2451605.
        assert_eq!(civil_from_jdn(2_451_605), (2000, 3, 1));
    }
}
