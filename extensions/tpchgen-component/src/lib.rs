//! Generate TPC-H benchmark data as DuckDB table functions, backed by the
//! pure-Rust `tpchgen` crate.
//!
//! Fixed reference tables (no argument):
//!   tpch_region() -> table(r_regionkey BIGINT, r_name VARCHAR, r_comment VARCHAR)   -- 5 rows
//!   tpch_nation() -> table(n_nationkey BIGINT, n_name VARCHAR, n_regionkey BIGINT,
//!                          n_comment VARCHAR)                                        -- 25 rows
//!
//! Scaled tables (sf DOUBLE; clamped to (0, MAX_SF]; NULL/<=0 -> zero rows):
//!   tpch_supplier(sf)  tpch_customer(sf)  tpch_part(sf)
//!   tpch_partsupp(sf)  tpch_orders(sf)    tpch_lineitem(sf)
//!
//! Scalar:
//!   tpch_query(n BIGINT) -> VARCHAR  -- SQL text of TPC-H query n (1..=22); else NULL
//!
//! Decimals are emitted as DOUBLE, dates as VARCHAR (YYYY-MM-DD), keys/sizes as
//! BIGINT, everything else as VARCHAR. NULL or invalid input -> zero rows (table)
//! or NULL (scalar); never a panic.
use std::collections::HashMap;
use std::sync::{
    atomic::{AtomicU32, Ordering},
    Mutex, OnceLock,
};

use wit_bindgen::rt::string::String;
use wit_bindgen::rt::vec::Vec;

wit_bindgen::generate!({ path: "./wit", world: "duckdb:extension/duckdb-extension" });

use duckdb::extension::{column_types as col, runtime, types};
use exports::duckdb::extension::{callback_dispatch, guest};

use tpchgen::generators::{
    CustomerGenerator, LineItemGenerator, NationGenerator, OrderGenerator, PartGenerator,
    PartSuppGenerator, RegionGenerator, SupplierGenerator,
};
use tpchgen::q_and_a::queries;

// Cap the scale factor so a runaway tpch_lineitem(sf) on wasm can't allocate
// without bound. sf=0.01 already yields ~60k lineitem rows; this is a generator
// demo, not a warehouse.
const MAX_SF: f64 = 0.05;

type Row = std::vec::Vec<types::Duckvalue>;
type Rows = std::vec::Vec<Row>;

struct Extension;

impl guest::Guest for Extension {
    fn load() -> Result<types::Loadresult, types::Duckerror> {
        register_all()?;
        Ok(types::Loadresult {
            name: "tpchgen".into(),
            version: Some(env!("CARGO_PKG_VERSION").into()),
            requires: vec![
                types::Capabilitykind::Table,
                types::Capabilitykind::Scalar,
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

// major-4 colvec <-> row adapter (tpchgen keeps its hand-written scalar + table
// fns; only the scalar HOT PATH is bridged to columnar).
datalink_extcore::__columnar_bridge_conv!(types, col);

impl callback_dispatch::Guest for Extension {
    // major-4 columnar scalar hot path: bridge colvec -> rows, delegate per-row
    // to the unchanged hand-written `call_scalar`, rebuild the result column.
    fn call_scalar_batch_col(
        handle: u32,
        args: Vec<callback_dispatch::Colvec>,
        ctx: types::Invokeinfo,
    ) -> Result<callback_dispatch::Colvec, types::Duckerror> {
        let base = ctx.rowindex.unwrap_or(0);
        let rows = __bridge_colvecs_to_rows(&args);
        let mut out = Vec::with_capacity(rows.len());
        for (i, a) in rows.into_iter().enumerate() {
            out.push(Self::call_scalar(
                handle,
                a,
                types::Invokeinfo {
                    rowindex: Some(base + i as u64),
                    iswindow: ctx.iswindow,
                },
            )?);
        }
        Ok(__bridge_vals_to_colvec(out))
    }

    fn call_aggregate_col(
        _handle: u32,
        _args: Vec<callback_dispatch::Colvec>,
    ) -> Result<types::Duckvalue, types::Duckerror> {
        Err(types::Duckerror::Unsupported("tpchgen: no aggs".into()))
    }

    fn call_cast_col(
        _handle: u32,
        _arg: callback_dispatch::Colvec,
    ) -> Result<callback_dispatch::Colvec, types::Duckerror> {
        Err(types::Duckerror::Unsupported("tpchgen: no casts".into()))
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
            .ok_or_else(|| types::Duckerror::Internal("unknown handle".into()))?;
        match which {
            F::Query => Ok(tpch_query(&args)),
            _ => Err(types::Duckerror::Internal(
                "non-scalar handle dispatched as scalar".into(),
            )),
        }
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
            .ok_or_else(|| types::Duckerror::Internal("unknown handle".into()))?;
        let rows = match which {
            F::Region => region_rows(),
            F::Nation => nation_rows(),
            F::Supplier => supplier_rows(sf_arg(&args)),
            F::Customer => customer_rows(sf_arg(&args)),
            F::Part => part_rows(sf_arg(&args)),
            F::PartSupp => partsupp_rows(sf_arg(&args)),
            F::Orders => orders_rows(sf_arg(&args)),
            F::LineItem => lineitem_rows(sf_arg(&args)),
            F::Query => {
                return Err(types::Duckerror::Internal(
                    "scalar handle dispatched as table".into(),
                ))
            }
        };
        Ok(rows.into())
    }

    fn call_pragma(
        _h: u32,
        _a: Vec<types::Duckvalue>,
    ) -> Result<Option<types::Duckvalue>, types::Duckerror> {
        Err(types::Duckerror::Unsupported("tpchgen: no pragmas".into()))
    }
    fn call_cast(
        _h: u32,
        _v: types::Duckvalue,
    ) -> Result<types::Duckvalue, types::Duckerror> {
        Err(types::Duckerror::Unsupported("tpchgen: no casts".into()))
    }
}

export!(Extension);

fn txt(s: impl ToString) -> types::Duckvalue {
    types::Duckvalue::Text(s.to_string().into())
}
fn i64v(v: i64) -> types::Duckvalue {
    types::Duckvalue::Int64(v)
}

// Parse a scale-factor argument. Accepts DOUBLE or INT; NULL/missing/invalid or
// <=0 -> None (caller emits zero rows). Clamps to MAX_SF.
fn sf_arg(args: &[types::Duckvalue]) -> Option<f64> {
    let raw = match args.first() {
        Some(types::Duckvalue::Float64(v)) => *v,
        Some(types::Duckvalue::Float32(v)) => *v as f64,
        Some(types::Duckvalue::Int64(v)) => *v as f64,
        Some(types::Duckvalue::Int32(v)) => *v as f64,
        _ => return None,
    };
    if !raw.is_finite() || raw <= 0.0 {
        return None;
    }
    Some(raw.min(MAX_SF))
}

fn region_rows() -> Rows {
    RegionGenerator::new(1.0, 1, 1)
        .iter()
        .map(|r| vec![i64v(r.r_regionkey), txt(r.r_name), txt(r.r_comment)])
        .collect()
}

fn nation_rows() -> Rows {
    NationGenerator::new(1.0, 1, 1)
        .iter()
        .map(|n| {
            vec![
                i64v(n.n_nationkey),
                txt(n.n_name),
                i64v(n.n_regionkey),
                txt(n.n_comment),
            ]
        })
        .collect()
}

fn supplier_rows(sf: Option<f64>) -> Rows {
    let Some(sf) = sf else { return Rows::new() };
    SupplierGenerator::new(sf, 1, 1)
        .iter()
        .map(|s| {
            vec![
                i64v(s.s_suppkey),
                txt(s.s_name),
                txt(s.s_address),
                i64v(s.s_nationkey),
                txt(s.s_phone),
                types::Duckvalue::Float64(s.s_acctbal.as_f64()),
                txt(s.s_comment),
            ]
        })
        .collect()
}

fn customer_rows(sf: Option<f64>) -> Rows {
    let Some(sf) = sf else { return Rows::new() };
    CustomerGenerator::new(sf, 1, 1)
        .iter()
        .map(|c| {
            vec![
                i64v(c.c_custkey),
                txt(c.c_name),
                txt(c.c_address),
                i64v(c.c_nationkey),
                txt(c.c_phone),
                types::Duckvalue::Float64(c.c_acctbal.as_f64()),
                txt(c.c_mktsegment),
                txt(c.c_comment),
            ]
        })
        .collect()
}

fn part_rows(sf: Option<f64>) -> Rows {
    let Some(sf) = sf else { return Rows::new() };
    PartGenerator::new(sf, 1, 1)
        .iter()
        .map(|p| {
            vec![
                i64v(p.p_partkey),
                txt(p.p_name),
                txt(p.p_mfgr),
                txt(p.p_brand),
                txt(p.p_type),
                i64v(p.p_size as i64),
                txt(p.p_container),
                types::Duckvalue::Float64(p.p_retailprice.as_f64()),
                txt(p.p_comment),
            ]
        })
        .collect()
}

fn partsupp_rows(sf: Option<f64>) -> Rows {
    let Some(sf) = sf else { return Rows::new() };
    PartSuppGenerator::new(sf, 1, 1)
        .iter()
        .map(|ps| {
            vec![
                i64v(ps.ps_partkey),
                i64v(ps.ps_suppkey),
                i64v(ps.ps_availqty as i64),
                types::Duckvalue::Float64(ps.ps_supplycost.as_f64()),
                txt(ps.ps_comment),
            ]
        })
        .collect()
}

fn orders_rows(sf: Option<f64>) -> Rows {
    let Some(sf) = sf else { return Rows::new() };
    OrderGenerator::new(sf, 1, 1)
        .iter()
        .map(|o| {
            vec![
                i64v(o.o_orderkey),
                i64v(o.o_custkey),
                txt(o.o_orderstatus),
                types::Duckvalue::Float64(o.o_totalprice.as_f64()),
                txt(o.o_orderdate),
                txt(o.o_orderpriority),
                txt(o.o_clerk),
                i64v(o.o_shippriority as i64),
                txt(o.o_comment),
            ]
        })
        .collect()
}

fn lineitem_rows(sf: Option<f64>) -> Rows {
    let Some(sf) = sf else { return Rows::new() };
    LineItemGenerator::new(sf, 1, 1)
        .iter()
        .map(|l| {
            vec![
                i64v(l.l_orderkey),
                i64v(l.l_partkey),
                i64v(l.l_suppkey),
                i64v(l.l_linenumber as i64),
                i64v(l.l_quantity),
                types::Duckvalue::Float64(l.l_extendedprice.as_f64()),
                types::Duckvalue::Float64(l.l_discount.as_f64()),
                types::Duckvalue::Float64(l.l_tax.as_f64()),
                txt(l.l_returnflag),
                txt(l.l_linestatus),
                txt(l.l_shipdate),
                txt(l.l_commitdate),
                txt(l.l_receiptdate),
                txt(l.l_shipinstruct),
                txt(l.l_shipmode),
                txt(l.l_comment),
            ]
        })
        .collect()
}

fn tpch_query(args: &[types::Duckvalue]) -> types::Duckvalue {
    let n = match args.first() {
        Some(types::Duckvalue::Int64(v)) => *v,
        Some(types::Duckvalue::Int32(v)) => *v as i64,
        _ => return types::Duckvalue::Null,
    };
    if !(1..=22).contains(&n) {
        return types::Duckvalue::Null;
    }
    match queries::query(n as i32) {
        Some(sql) => types::Duckvalue::Text(sql.trim().into()),
        None => types::Duckvalue::Null,
    }
}

#[derive(Clone, Copy, PartialEq)]
enum F {
    Region,
    Nation,
    Supplier,
    Customer,
    Part,
    PartSupp,
    Orders,
    LineItem,
    Query,
}

static NEXT: AtomicU32 = AtomicU32::new(1);
static HANDLERS: OnceLock<Mutex<HashMap<u32, F>>> = OnceLock::new();
fn handlers() -> &'static Mutex<HashMap<u32, F>> {
    HANDLERS.get_or_init(|| Mutex::new(HashMap::new()))
}

fn col(name: &str, logical: types::Logicaltype) -> types::Columndef {
    types::Columndef {
        name: name.into(),
        logical,
    }
}

fn register_all() -> Result<(), types::Duckerror> {
    register_tables()?;
    register_scalar()?;
    Ok(())
}

fn register_tables() -> Result<(), types::Duckerror> {
    let cap = runtime::get_capability(types::Capabilitykind::Table)
        .ok_or_else(|| types::Duckerror::Internal("no table capability".into()))?;
    let reg = match cap {
        runtime::Capability::Table(r) => r,
        _ => return Err(types::Duckerror::Internal("bad capability".into())),
    };

    use types::Logicaltype::{Float64, Int64, Text};
    let sf_arg = || {
        vec![runtime::Funcarg {
            name: Some("sf".into()),
            logical: Float64,
        }]
    };
    let no_arg = || std::vec::Vec::<runtime::Funcarg>::new();

    // (name, F, args, columns)
    let region_cols = vec![
        col("r_regionkey", Int64),
        col("r_name", Text),
        col("r_comment", Text),
    ];
    register_table(&reg, "tpch_region", F::Region, &no_arg(), &region_cols)?;

    let nation_cols = vec![
        col("n_nationkey", Int64),
        col("n_name", Text),
        col("n_regionkey", Int64),
        col("n_comment", Text),
    ];
    register_table(&reg, "tpch_nation", F::Nation, &no_arg(), &nation_cols)?;

    let supplier_cols = vec![
        col("s_suppkey", Int64),
        col("s_name", Text),
        col("s_address", Text),
        col("s_nationkey", Int64),
        col("s_phone", Text),
        col("s_acctbal", Float64),
        col("s_comment", Text),
    ];
    register_table(&reg, "tpch_supplier", F::Supplier, &sf_arg(), &supplier_cols)?;

    let customer_cols = vec![
        col("c_custkey", Int64),
        col("c_name", Text),
        col("c_address", Text),
        col("c_nationkey", Int64),
        col("c_phone", Text),
        col("c_acctbal", Float64),
        col("c_mktsegment", Text),
        col("c_comment", Text),
    ];
    register_table(&reg, "tpch_customer", F::Customer, &sf_arg(), &customer_cols)?;

    let part_cols = vec![
        col("p_partkey", Int64),
        col("p_name", Text),
        col("p_mfgr", Text),
        col("p_brand", Text),
        col("p_type", Text),
        col("p_size", Int64),
        col("p_container", Text),
        col("p_retailprice", Float64),
        col("p_comment", Text),
    ];
    register_table(&reg, "tpch_part", F::Part, &sf_arg(), &part_cols)?;

    let partsupp_cols = vec![
        col("ps_partkey", Int64),
        col("ps_suppkey", Int64),
        col("ps_availqty", Int64),
        col("ps_supplycost", Float64),
        col("ps_comment", Text),
    ];
    register_table(&reg, "tpch_partsupp", F::PartSupp, &sf_arg(), &partsupp_cols)?;

    let orders_cols = vec![
        col("o_orderkey", Int64),
        col("o_custkey", Int64),
        col("o_orderstatus", Text),
        col("o_totalprice", Float64),
        col("o_orderdate", Text),
        col("o_orderpriority", Text),
        col("o_clerk", Text),
        col("o_shippriority", Int64),
        col("o_comment", Text),
    ];
    register_table(&reg, "tpch_orders", F::Orders, &sf_arg(), &orders_cols)?;

    let lineitem_cols = vec![
        col("l_orderkey", Int64),
        col("l_partkey", Int64),
        col("l_suppkey", Int64),
        col("l_linenumber", Int64),
        col("l_quantity", Int64),
        col("l_extendedprice", Float64),
        col("l_discount", Float64),
        col("l_tax", Float64),
        col("l_returnflag", Text),
        col("l_linestatus", Text),
        col("l_shipdate", Text),
        col("l_commitdate", Text),
        col("l_receiptdate", Text),
        col("l_shipinstruct", Text),
        col("l_shipmode", Text),
        col("l_comment", Text),
    ];
    register_table(&reg, "tpch_lineitem", F::LineItem, &sf_arg(), &lineitem_cols)?;

    Ok(())
}

fn register_table(
    reg: &runtime::TableRegistry,
    name: &str,
    f: F,
    args: &[runtime::Funcarg],
    columns: &[types::Columndef],
) -> Result<(), types::Duckerror> {
    let h = NEXT.fetch_add(1, Ordering::Relaxed);
    handlers().lock().unwrap().insert(h, f);
    let opts = runtime::Extopts {
        description: Some(format!("TPC-H generator: {name}")),
        tags: vec!["tpch".into(), "benchmark".into()],
    };
    reg.register(
        name,
        args,
        columns,
        runtime::TableCallback::new(h),
        Some(&opts),
    )?;
    Ok(())
}

fn register_scalar() -> Result<(), types::Duckerror> {
    let cap = runtime::get_capability(types::Capabilitykind::Scalar)
        .ok_or_else(|| types::Duckerror::Internal("no scalar capability".into()))?;
    let reg = match cap {
        runtime::Capability::Scalar(r) => r,
        _ => return Err(types::Duckerror::Internal("bad capability".into())),
    };
    let h = NEXT.fetch_add(1, Ordering::Relaxed);
    handlers().lock().unwrap().insert(h, F::Query);
    let det = types::Funcflags::DETERMINISTIC | types::Funcflags::STATELESS;
    reg.register(
        "tpch_query",
        &[runtime::Funcarg {
            name: Some("n".into()),
            logical: types::Logicaltype::Int64,
        }],
        &types::Logicaltype::Text,
        runtime::ScalarCallback::new(h),
        Some(&runtime::Funcopts {
            description: Some("SQL text of TPC-H query n (1..=22)".into()),
            tags: vec!["tpch".into()],
            attributes: det,
        }),
    )?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn as_text(v: &types::Duckvalue) -> Option<&str> {
        match v {
            types::Duckvalue::Text(s) => Some(s.as_str()),
            _ => None,
        }
    }
    fn as_i64(v: &types::Duckvalue) -> Option<i64> {
        match v {
            types::Duckvalue::Int64(n) => Some(*n),
            _ => None,
        }
    }

    #[test]
    fn region_is_five_fixed_rows() {
        let rows = region_rows();
        assert_eq!(rows.len(), 5);
        assert_eq!(as_i64(&rows[0][0]), Some(0));
        assert_eq!(as_text(&rows[0][1]), Some("AFRICA"));
        assert_eq!(as_i64(&rows[1][0]), Some(1));
        assert_eq!(as_text(&rows[1][1]), Some("AMERICA"));
    }

    #[test]
    fn nation_is_twentyfive_fixed_rows() {
        let rows = nation_rows();
        assert_eq!(rows.len(), 25);
        // nation 0 = ALGERIA, region 0 (AFRICA)
        assert_eq!(as_i64(&rows[0][0]), Some(0));
        assert_eq!(as_text(&rows[0][1]), Some("ALGERIA"));
        assert_eq!(as_i64(&rows[0][2]), Some(0));
    }

    #[test]
    fn null_or_invalid_sf_yields_zero_rows() {
        assert!(lineitem_rows(sf_arg(&[types::Duckvalue::Null])).is_empty());
        assert!(orders_rows(sf_arg(&[])).is_empty());
        assert!(part_rows(sf_arg(&[types::Duckvalue::Float64(0.0)])).is_empty());
        assert!(customer_rows(sf_arg(&[types::Duckvalue::Float64(-1.0)])).is_empty());
    }

    #[test]
    fn scaled_tables_produce_rows_at_small_sf() {
        let sf = sf_arg(&[types::Duckvalue::Float64(0.01)]);
        assert!(sf.is_some());
        // SF 0.01: 1500 customers, 100 suppliers, 2000 parts (base counts * 0.01).
        assert_eq!(customer_rows(sf).len(), 1500);
        assert_eq!(supplier_rows(sf).len(), 100);
        assert_eq!(part_rows(sf).len(), 2000);
        assert!(!lineitem_rows(sf).is_empty());
    }

    #[test]
    fn sf_is_clamped() {
        // A huge sf is clamped to MAX_SF, not honored literally.
        assert_eq!(sf_arg(&[types::Duckvalue::Float64(1000.0)]), Some(MAX_SF));
    }

    #[test]
    fn query_text_and_bounds() {
        let q1 = tpch_query(&[types::Duckvalue::Int64(1)]);
        match q1 {
            types::Duckvalue::Text(s) => assert!(s.to_lowercase().contains("lineitem")),
            _ => panic!("expected query text"),
        }
        assert!(matches!(
            tpch_query(&[types::Duckvalue::Int64(0)]),
            types::Duckvalue::Null
        ));
        assert!(matches!(
            tpch_query(&[types::Duckvalue::Int64(23)]),
            types::Duckvalue::Null
        ));
        assert!(matches!(
            tpch_query(&[types::Duckvalue::Null]),
            types::Duckvalue::Null
        ));
    }
}
