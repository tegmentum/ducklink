//! Submission-quality coverage for the native ducklink Direction-2 bridge.
//!
//! These tests load real prebuilt `duckdb:extension` wasm components from the
//! corpus (`artifacts/extensions/<name>.wasm`) into an in-process DuckDB through
//! the crate's public API (`Engine2` + `register_components` /
//! `register_scalars` / `register_tables`) and assert concrete results, so they
//! exercise the marshalling for every logical type, NULL handling, multi-arg
//! calls, the table-function vector chunking, the raw-C-API aggregate path
//! (init/update/combine/finalize/destroy + per-group state), multi-component
//! registration, the no-raw-connection skip path, error surfacing, and
//! concurrency.
//!
//! Only pure-compute, deterministic components are used (no dns/http), so the
//! suite is reproducible. The whole file is a no-op without the `bundled`
//! feature (which provides the in-process DuckDB to register into).
#![cfg(feature = "bundled")]

use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use duckdb::Connection;

use ducklink::engine::Engine2;
use ducklink::reg_duckdb::{
    register_components, register_scalars, register_tables, ComponentSpec,
};

// ---------------------------------------------------------------------------
// Shared setup
// ---------------------------------------------------------------------------

/// Path to a prebuilt corpus artifact by extension name.
fn artifact(name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../artifacts/extensions")
        .join(format!("{name}.wasm"))
}

/// A live in-process DuckDB plus the raw sibling connection aggregate
/// registration needs. Created via the raw C API so we hold both a duckdb-rs
/// `Connection` (for scalars/tables/queries) and a raw `duckdb_connection` (for
/// aggregates) on the *same* database — functions registered on either are
/// visible to both. Closing the handles is deferred to `Drop`.
struct Db {
    con: Connection,
    raw_con: duckdb::ffi::duckdb_connection,
    db: duckdb::ffi::duckdb_database,
}

impl Db {
    fn new() -> Self {
        let mut db: duckdb::ffi::duckdb_database = std::ptr::null_mut();
        assert_eq!(
            unsafe { duckdb::ffi::duckdb_open(std::ptr::null(), &mut db) },
            duckdb::ffi::DuckDBSuccess,
            "duckdb_open"
        );
        let con = unsafe { Connection::open_from_raw(db) }.expect("open_from_raw");
        let mut raw_con: duckdb::ffi::duckdb_connection = std::ptr::null_mut();
        assert_eq!(
            unsafe { duckdb::ffi::duckdb_connect(db, &mut raw_con) },
            duckdb::ffi::DuckDBSuccess,
            "duckdb_connect"
        );
        Db { con, raw_con, db }
    }
}

impl Drop for Db {
    fn drop(&mut self) {
        // Drop the duckdb-rs connection first (it owns one ref on the db), then
        // the raw sibling, then the database.
        unsafe {
            duckdb::ffi::duckdb_disconnect(&mut self.raw_con);
            duckdb::ffi::duckdb_close(&mut self.db);
        }
    }
}

/// Build a `Db`, load the named components into a shared engine, and register
/// all their functions (scalars + tables on the duckdb-rs connection, aggregates
/// on the raw sibling). Returns the live db (keep it in scope: it owns the
/// engine `Arc` that keeps the loaded wasm components alive) and the total
/// function count registered.
fn setup_with(names: &[&str]) -> (Db, usize) {
    let db = Db::new();
    let engine = Arc::new(Mutex::new(Engine2::new().expect("engine")));
    let specs: Vec<ComponentSpec> = names
        .iter()
        .map(|n| ComponentSpec {
            name: (*n).to_string(),
            path: artifact(n),
        })
        .collect();
    let n = register_components(&db.con, Some(db.raw_con), engine, &specs)
        .expect("register components");
    (db, n)
}

/// Convenience for the common single-component case.
fn setup(name: &str) -> (Db, usize) {
    setup_with(&[name])
}

// ---------------------------------------------------------------------------
// 1. Scalar type coverage — one test per logical type, concrete values + TYPE
// ---------------------------------------------------------------------------

#[test]
fn scalar_int64_value_and_type() {
    let (db, _) = setup("sample_extension");
    let v: i64 = db
        .con
        .query_row("SELECT sample_plus_one(41)", [], |r| r.get(0))
        .expect("query");
    assert_eq!(v, 42);

    // Column TYPE is BIGINT.
    let ty: String = db
        .con
        .query_row(
            "SELECT typeof(sample_plus_one(1))",
            [],
            |r| r.get(0),
        )
        .expect("typeof");
    assert_eq!(ty, "BIGINT");
}

#[test]
fn scalar_boolean_value_and_type() {
    let (db, _) = setup("isin");
    // Valid ISIN -> true.
    let good: bool = db
        .con
        .query_row("SELECT isin_validate('US0378331005')", [], |r| r.get(0))
        .expect("good");
    assert!(good, "valid Apple ISIN validates true");

    // Malformed input -> false (validator, not NULL here).
    let junk: bool = db
        .con
        .query_row("SELECT isin_validate('not an isin')", [], |r| r.get(0))
        .expect("junk");
    assert!(!junk, "garbage validates false");

    let ty: String = db
        .con
        .query_row("SELECT typeof(isin_validate('US0378331005'))", [], |r| {
            r.get(0)
        })
        .expect("typeof");
    assert_eq!(ty, "BOOLEAN");
}

#[test]
fn scalar_text_roundtrips() {
    let (db, _) = setup("rot13");
    // rot13 is its own inverse: applying twice round-trips.
    let s: String = db
        .con
        .query_row("SELECT rot13(rot13('roundtrip'))", [], |r| r.get(0))
        .expect("query");
    assert_eq!(s, "roundtrip");

    let enc: String = db
        .con
        .query_row("SELECT rot13('Hello, World!')", [], |r| r.get(0))
        .expect("enc");
    assert_eq!(enc, "Uryyb, Jbeyq!");

    let ty: String = db
        .con
        .query_row("SELECT typeof(rot13('x'))", [], |r| r.get(0))
        .expect("typeof");
    assert_eq!(ty, "VARCHAR");
}

#[test]
fn scalar_float64_value_and_type() {
    let (db, _) = setup("haversine");
    // NYC -> LA great-circle distance ~ 3936 km.
    let km: f64 = db
        .con
        .query_row(
            "SELECT round(haversine_km(40.7128, -74.0060, 34.0522, -118.2437), 0)",
            [],
            |r| r.get(0),
        )
        .expect("km");
    assert_eq!(km, 3936.0);

    // Identical points -> 0 distance.
    let zero: f64 = db
        .con
        .query_row("SELECT haversine_km(0, 0, 0, 0)", [], |r| r.get(0))
        .expect("zero");
    assert_eq!(zero, 0.0);

    let ty: String = db
        .con
        .query_row(
            "SELECT typeof(haversine_km(1.0, 2.0, 3.0, 4.0))",
            [],
            |r| r.get(0),
        )
        .expect("typeof");
    assert_eq!(ty, "DOUBLE");
}

#[test]
fn scalar_blob_encode_decode_roundtrip() {
    let (db, _) = setup("baseN");
    // base32_encode(BLOB) -> VARCHAR.
    let enc: String = db
        .con
        .query_row("SELECT base32_encode('Hello'::BLOB)", [], |r| r.get(0))
        .expect("b32enc");
    assert_eq!(enc, "JBSWY3DP");

    // base32_decode(VARCHAR) -> BLOB; decoding the encoding round-trips bytes.
    let dec: Vec<u8> = db
        .con
        .query_row("SELECT base32_decode('JBSWY3DP')", [], |r| r.get(0))
        .expect("b32dec");
    assert_eq!(dec, b"Hello");

    let ty: String = db
        .con
        .query_row("SELECT typeof(base32_decode('JBSWY3DP'))", [], |r| r.get(0))
        .expect("typeof");
    assert_eq!(ty, "BLOB");
}

// ---------------------------------------------------------------------------
// 2. NULL handling — validator returning SQL NULL, and NULL passed in
// ---------------------------------------------------------------------------

#[test]
fn null_returned_on_bad_input() {
    let (db, _) = setup("baseN");
    // base58_decode of input with non-alphabet chars returns SQL NULL (not an
    // error). Asserts the bridge's set_null path produces a real SQL NULL.
    let dec: Option<Vec<u8>> = db
        .con
        .query_row("SELECT base58_decode('invalid0Il')", [], |r| r.get(0))
        .expect("query");
    assert!(dec.is_none(), "bad base58 input yields SQL NULL");
}

#[test]
fn null_argument_propagates() {
    let (db, _) = setup("haversine");
    // A NULL argument flows through the bridge as DuckValue::Null and the
    // component returns NULL.
    let v: Option<f64> = db
        .con
        .query_row("SELECT haversine_km(1, 2, 3, NULL)", [], |r| r.get(0))
        .expect("query");
    assert!(v.is_none(), "NULL argument yields NULL result");
}

// ---------------------------------------------------------------------------
// 3. Multi-arg scalar
// ---------------------------------------------------------------------------

#[test]
fn multi_arg_scalar_haversine() {
    let (db, _) = setup("haversine");
    // 4-arg float scalar.
    let mi: f64 = db
        .con
        .query_row(
            "SELECT round(haversine_mi(40.7128, -74.0060, 34.0522, -118.2437), 0)",
            [],
            |r| r.get(0),
        )
        .expect("mi");
    assert_eq!(mi, 2446.0);
}

#[test]
fn multi_arg_scalar_bloom_contains() {
    let (db, _) = setup("bloom");
    // 2-arg (VARCHAR, VARCHAR) -> BOOL scalar, using a fixed filter hex built
    // from a single item via the scalar path only (no aggregate here).
    // Build a filter for one element through the aggregate over a single row.
    let has: bool = db
        .con
        .query_row(
            "WITH f AS (SELECT bloom_filter(v) AS bf FROM (VALUES ('apple')) t(v)) \
             SELECT bloom_contains(bf, 'apple') FROM f",
            [],
            |r| r.get(0),
        )
        .expect("has");
    assert!(has, "bloom_contains reports the inserted item present");
}

// ---------------------------------------------------------------------------
// 4. Table functions — 0 rows, large (>2048) chunked, sum/count
// ---------------------------------------------------------------------------

#[test]
fn table_zero_rows() {
    let (db, _) = setup("sample_extension");
    let count: i64 = db
        .con
        .query_row("SELECT count(*) FROM sample_emit_sequence(0)", [], |r| {
            r.get(0)
        })
        .expect("count");
    assert_eq!(count, 0, "limit 0 emits no rows");
}

#[test]
fn table_large_chunked() {
    let (db, _) = setup("sample_extension");
    // 5000 > the 2048 vector size, so `func` streams across multiple chunks.
    let count: i64 = db
        .con
        .query_row("SELECT count(*) FROM sample_emit_sequence(5000)", [], |r| {
            r.get(0)
        })
        .expect("count");
    assert_eq!(count, 5000, "exactly 5000 rows across multiple chunks");

    // sum(0..5000) = 5000*4999/2.
    let sum: i64 = db
        .con
        .query_row("SELECT sum(value) FROM sample_emit_sequence(5000)", [], |r| {
            r.get(0)
        })
        .expect("sum");
    assert_eq!(sum, 5000 * 4999 / 2);

    // Exact boundary multiples of the chunk size also stream correctly.
    let exact: i64 = db
        .con
        .query_row("SELECT count(*) FROM sample_emit_sequence(4096)", [], |r| {
            r.get(0)
        })
        .expect("exact");
    assert_eq!(exact, 4096);
}

#[test]
fn table_column_name_and_values() {
    let (db, _) = setup("sample_extension");
    // The single result column is named `value`; values are 0..limit in order.
    let mut stmt = db
        .con
        .prepare("SELECT value FROM sample_emit_sequence(5) ORDER BY value")
        .expect("prepare");
    let rows: Vec<i64> = stmt
        .query_map([], |r| r.get::<_, i64>(0))
        .expect("query_map")
        .map(|r| r.unwrap())
        .collect();
    assert_eq!(rows, vec![0, 1, 2, 3, 4]);
    assert_eq!(stmt.column_names(), vec!["value".to_string()]);
}

// ---------------------------------------------------------------------------
// 5. Aggregates — basic, GROUP BY, empty, large, VARCHAR round-trip, sample_sum
// ---------------------------------------------------------------------------

#[test]
fn aggregate_basic_harmonic_mean() {
    let (db, _) = setup("aggstat");
    // harmonic_mean(1,2,4) = 3 / (1 + 0.5 + 0.25) = 1.714286.
    let hm: f64 = db
        .con
        .query_row(
            "SELECT round(harmonic_mean(x), 6) FROM (VALUES (1.0),(2.0),(4.0)) t(x)",
            [],
            |r| r.get(0),
        )
        .expect("hm");
    assert_eq!(hm, 1.714286);
}

#[test]
fn aggregate_group_by_per_group_state() {
    let (db, _) = setup("aggstat");
    // Two groups, each with its own harmonic mean — exercises per-group boxed
    // state. Group 'a': {1,2,4} -> 1.714286; group 'b': {2,8} -> 3.2.
    let mut stmt = db
        .con
        .prepare(
            "SELECT g, round(harmonic_mean(x), 6) FROM \
             (VALUES ('a',1.0),('a',2.0),('a',4.0),('b',2.0),('b',8.0)) t(g,x) \
             GROUP BY g ORDER BY g",
        )
        .expect("prepare");
    let rows: Vec<(String, f64)> = stmt
        .query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, f64>(1)?)))
        .expect("query_map")
        .map(|r| r.unwrap())
        .collect();
    assert_eq!(rows, vec![("a".to_string(), 1.714286), ("b".to_string(), 3.2)]);
}

#[test]
fn aggregate_empty_input_is_null() {
    let (db, _) = setup("aggstat");
    // Over zero rows the component returns NULL (no values -> undefined mean).
    let v: Option<f64> = db
        .con
        .query_row(
            "SELECT harmonic_mean(x) FROM (SELECT 1.0 x WHERE false) t",
            [],
            |r| r.get(0),
        )
        .expect("query");
    assert!(v.is_none(), "harmonic_mean over no rows is NULL");
}

#[test]
fn aggregate_large_input_multi_chunk() {
    let (db, _) = setup("sample_extension");
    // sample_sum(INT64): 10000 rows > 2048, so `update` runs over many input
    // chunks (and DuckDB may parallelize -> `combine`). sum(1..=10000).
    let sum: i64 = db
        .con
        .query_row(
            "SELECT sample_sum(i) FROM range(1, 10001) t(i)",
            [],
            |r| r.get(0),
        )
        .expect("sum");
    assert_eq!(sum, 10000 * 10001 / 2);
}

#[test]
fn aggregate_sample_sum_basic_and_empty() {
    let (db, _) = setup("sample_extension");
    let sum: i64 = db
        .con
        .query_row("SELECT sample_sum(i) FROM range(1, 6) t(i)", [], |r| {
            r.get(0)
        })
        .expect("sum");
    assert_eq!(sum, 1 + 2 + 3 + 4 + 5);

    // Empty input -> 0 (sample_sum's identity), not NULL.
    let empty: i64 = db
        .con
        .query_row(
            "SELECT sample_sum(i) FROM (SELECT 1 i WHERE false) t",
            [],
            |r| r.get(0),
        )
        .expect("empty");
    assert_eq!(empty, 0);
}

#[test]
fn aggregate_varchar_roundtrip_through_scalar() {
    let (db, _) = setup("bloom");
    // VARCHAR-returning aggregate `bloom_filter` -> companion scalar
    // `bloom_contains`. Build a filter over three fruits, then probe membership.
    let (has_apple, has_banana, has_durian): (bool, bool, bool) = db
        .con
        .query_row(
            "WITH f AS (SELECT bloom_filter(v) AS bf \
                        FROM (VALUES ('apple'),('banana'),('cherry')) t(v)) \
             SELECT bloom_contains(bf, 'apple'), \
                    bloom_contains(bf, 'banana'), \
                    bloom_contains(bf, 'durian') FROM f",
            [],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )
        .expect("query");
    assert!(has_apple, "inserted item present");
    assert!(has_banana, "inserted item present");
    assert!(!has_durian, "absent item reported absent (no false negatives)");
}

// ---------------------------------------------------------------------------
// 6. Multiple components in one DB
// ---------------------------------------------------------------------------

#[test]
fn multiple_components_one_db() {
    // Load two different extensions into the same connection; both work.
    let (db, _) = setup_with(&["rot13", "slug"]);

    let r: String = db
        .con
        .query_row("SELECT rot13('abc')", [], |r| r.get(0))
        .expect("rot13");
    assert_eq!(r, "nop");

    let s: String = db
        .con
        .query_row("SELECT slugify('Hello, World!')", [], |r| r.get(0))
        .expect("slug");
    assert_eq!(s, "hello-world");

    // And a single statement using both.
    let combined: bool = db
        .con
        .query_row("SELECT rot13('x') = 'k' AND slugify('A B') = 'a-b'", [], |r| {
            r.get(0)
        })
        .expect("combined");
    assert!(combined);
}

// ---------------------------------------------------------------------------
// 7. Error / unsupported paths
// ---------------------------------------------------------------------------

#[test]
fn no_raw_connection_skips_aggregates_registers_rest() {
    // With raw_con = None the aggregate path is skipped (with a note); this is
    // exactly what the loadable entry point does (it has no raw connection).
    //
    // Part A — a component whose ONLY function is an aggregate (aggstat ships
    // just harmonic_mean): register_components returns 0 (aggregate skipped, no
    // scalars/tables) and does NOT panic, and the function is absent from the
    // catalog so calling it errors.
    let db = Db::new();
    let engine = Arc::new(Mutex::new(Engine2::new().expect("engine")));
    let agg_only = vec![ComponentSpec {
        name: "aggstat".to_string(),
        path: artifact("aggstat"),
    }];
    let n_agg = register_components(&db.con, None, engine, &agg_only).expect("register aggstat");
    assert_eq!(
        n_agg, 0,
        "aggregate-only component registers nothing without a raw connection"
    );
    let err = db
        .con
        .query_row("SELECT harmonic_mean(1.0)", [], |r| r.get::<_, f64>(0));
    assert!(err.is_err(), "skipped aggregate is not registered");

    // Part B — sample_extension has a scalar + a table + an aggregate. With
    // raw_con = None the scalar and table register (count >= 2) but the
    // aggregate is skipped, so sample_sum is NOT callable while sample_plus_one
    // IS. This proves the skip is surgical, not all-or-nothing.
    let db2 = Db::new();
    let engine2 = Arc::new(Mutex::new(Engine2::new().expect("engine")));
    let mixed = vec![ComponentSpec {
        name: "sample_extension".to_string(),
        path: artifact("sample_extension"),
    }];
    let n_mixed = register_components(&db2.con, None, engine2, &mixed).expect("register sample");
    assert!(
        n_mixed >= 2,
        "scalar + table registered without raw_con, got {n_mixed}"
    );
    let v: i64 = db2
        .con
        .query_row("SELECT sample_plus_one(1)", [], |r| r.get(0))
        .expect("scalar still works");
    assert_eq!(v, 2);
    let agg_err = db2
        .con
        .query_row("SELECT sample_sum(i) FROM range(3) t(i)", [], |r| {
            r.get::<_, i64>(0)
        });
    assert!(agg_err.is_err(), "aggregate skipped when raw_con is None");
}

#[test]
fn register_scalars_tables_counts_directly() {
    // Drive the lower-level public API directly: load a component and register
    // its scalars and tables, asserting the returned counts.
    let mut engine = Engine2::new().expect("engine");
    let loaded = engine
        .load("sample_extension", &artifact("sample_extension"))
        .expect("load");
    let engine = Arc::new(Mutex::new(engine));
    let con = Connection::open_in_memory().expect("open");

    let scalars = register_scalars(&con, engine.clone(), &loaded.scalars).expect("scalars");
    let tables = register_tables(&con, engine.clone(), &loaded.tables).expect("tables");
    assert!(scalars >= 1, "sample has >=1 scalar, got {scalars}");
    assert!(tables >= 1, "sample has >=1 table fn, got {tables}");

    // Sanity: a registered scalar and table both work on this connection.
    let v: i64 = con
        .query_row("SELECT sample_plus_one(0)", [], |r| r.get(0))
        .expect("scalar");
    assert_eq!(v, 1);
    let c: i64 = con
        .query_row("SELECT count(*) FROM sample_emit_sequence(3)", [], |r| {
            r.get(0)
        })
        .expect("table");
    assert_eq!(c, 3);
}

#[test]
fn dispatch_error_surfaces_as_duckdb_error() {
    let (db, _) = setup("sample_extension");
    // sample_emit_sequence rejects a negative argument inside the component;
    // the bridge surfaces that as a DuckDB error rather than panicking.
    let res = db
        .con
        .query_row("SELECT count(*) FROM sample_emit_sequence(-1)", [], |r| {
            r.get::<_, i64>(0)
        });
    assert!(
        res.is_err(),
        "negative limit surfaces as a query error, not a crash"
    );
}

// ---------------------------------------------------------------------------
// 8. Concurrency — worker threads invoke the bridge in parallel
// ---------------------------------------------------------------------------

#[test]
fn concurrent_scalar_under_threads() {
    let (db, _) = setup("sample_extension");
    db.con
        .execute_batch("SET threads=4;")
        .expect("set threads");

    // A large scalar query so DuckDB schedules the per-row dispatch across
    // worker threads (validates Mutex<Engine2> under parallelism).
    let sum: i64 = db
        .con
        .query_row(
            "SELECT sum(sample_plus_one(i)) FROM range(0, 20000) t(i)",
            [],
            |r| r.get(0),
        )
        .expect("sum");
    // sum_{i=0}^{19999} (i+1) = sum_{k=1}^{20000} k = 20000*20001/2.
    assert_eq!(sum, 20000 * 20001 / 2);
}

#[test]
fn concurrent_grouped_aggregate_under_threads() {
    let (db, _) = setup("sample_extension");
    db.con
        .execute_batch("SET threads=4;")
        .expect("set threads");

    // Many groups over many rows under parallelism: exercises per-group state
    // plus `combine` when DuckDB merges partial aggregates across threads.
    // Group key = i % 10; each group g gets the sum of all i in [0,20000) with
    // i % 10 == g.
    let mut stmt = db
        .con
        .prepare(
            "SELECT i % 10 AS g, sample_sum(i) FROM range(0, 20000) t(i) \
             GROUP BY g ORDER BY g",
        )
        .expect("prepare");
    let rows: Vec<(i64, i64)> = stmt
        .query_map([], |r| Ok((r.get::<_, i64>(0)?, r.get::<_, i64>(1)?)))
        .expect("query_map")
        .map(|r| r.unwrap())
        .collect();
    assert_eq!(rows.len(), 10, "ten groups");

    // Expected per-group sums computed independently.
    for (g, got) in rows {
        let expected: i64 = (0..20000).filter(|i| i % 10 == g).sum();
        assert_eq!(got, expected, "group {g} sum");
    }
}

// ---------------------------------------------------------------------------
// 9. Entry-point path — register_components end-to-end with all three kinds
// ---------------------------------------------------------------------------

#[test]
fn register_components_all_kinds_end_to_end() {
    // sample_extension declares a scalar, a table function, and an aggregate.
    // With a raw connection, register_components wires up all three.
    let (db, n) = setup("sample_extension");
    assert!(n >= 3, "scalar + table + aggregate registered, got {n}");

    // Scalar.
    let s: i64 = db
        .con
        .query_row("SELECT sample_plus_one(10)", [], |r| r.get(0))
        .expect("scalar");
    assert_eq!(s, 11);
    // Table.
    let t: i64 = db
        .con
        .query_row("SELECT count(*) FROM sample_emit_sequence(7)", [], |r| {
            r.get(0)
        })
        .expect("table");
    assert_eq!(t, 7);
    // Aggregate.
    let a: i64 = db
        .con
        .query_row("SELECT sample_sum(i) FROM range(1, 5) t(i)", [], |r| {
            r.get(0)
        })
        .expect("agg");
    assert_eq!(a, 1 + 2 + 3 + 4);
}
