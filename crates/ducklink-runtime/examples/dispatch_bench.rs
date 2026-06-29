//! Repeatable micro-benchmark harness for the dispatch hot paths, with no extra
//! dependencies (uses `std::time::Instant`). Run with:
//!
//!     cargo run --release -p ducklink-runtime --example dispatch_bench
//!
//! It exercises the parts of the per-row / per-call dispatch that execute on the
//! Rust side of the host->extension boundary WITHOUT instantiating a wasm
//! component (the wasm re-entry + Cranelift compile cost is covered separately by
//! the host's compile cache and is not what this harness measures). The targets:
//!
//!   1. callback-registry handle resolution -- run once PER ROW on the direct
//!      `dispatch_scalar` path. The pre-opt impl cloned the whole `CallbackEntry`
//!      whose `extension` was a `String`: a heap allocation per row just to read
//!      a u32 + an enum. The opt does two things -- (a) `extension` is now an
//!      `Arc<str>` so even a full-entry clone is a refcount bump, and (b) the
//!      per-row path borrows via `resolve` instead of cloning at all.
//!   2. value marshalling (the structurally-identical enum reconstruction the
//!      host runs for every arg in + every result out) -- representative of the
//!      `convert_*_duckvalue_*` functions, including the nested `Complex` arm.
//!
//! The harness prints ns/op so before/after numbers are directly comparable.

use std::hint::black_box;
use std::time::Instant;

use ducklink_runtime::{CallbackKind, CallbackRegistry};

fn bench<F: FnMut()>(name: &str, iters: u64, mut f: F) {
    // Warm up.
    for _ in 0..(iters / 10).max(1) {
        f();
    }
    let start = Instant::now();
    for _ in 0..iters {
        f();
    }
    let elapsed = start.elapsed();
    let ns = elapsed.as_nanos() as f64 / iters as f64;
    println!("{name:<48} {ns:>10.2} ns/op   ({iters} iters)");
}

fn main() {
    println!("dispatch hot-path micro-benchmarks (release)\n");

    // --- 1. callback-registry resolution (per row on the direct path) ---------
    let mut registry = CallbackRegistry::new();
    // Populate as a realistic load would: a handful of extensions each with a
    // few scalar/table/aggregate callbacks.
    let mut handles = Vec::new();
    for ext in ["isin", "luhn", "uuidx", "url", "email", "crypto"] {
        for k in [CallbackKind::Scalar, CallbackKind::Table, CallbackKind::Aggregate] {
            handles.push(registry.allocate_quiet(ext, k, 7));
        }
    }
    let hot = handles[0]; // a scalar handle, the one the loop hammers.

    let iters = 5_000_000u64;

    // Cloning resolution (the historical per-row cost).
    bench("registry.get (clone CallbackEntry)", iters, || {
        let entry = registry.get(black_box(hot)).unwrap();
        black_box(entry.dispatcher_handle);
        black_box(entry.kind);
    });

    // Borrowing resolution (the optimized per-row path): reads handle+kind with
    // no allocation, then borrows the extension name only when needed.
    bench("registry.resolve (borrow, no alloc)", iters, || {
        let entry = registry.resolve(black_box(hot)).unwrap();
        black_box(entry.dispatcher_handle);
        black_box(entry.kind);
        black_box(entry.extension.len());
    });

    // The dispatch site still needs the extension NAME to index
    // `self.extensions: HashMap<_, ExtensionInstance>`. Compare paying a `String`
    // clone (heap alloc + copy) vs an `Arc<str>` clone (atomic refcount bump) for
    // that per-row name handoff.
    let name_string: String = "postgres_scanner".to_string();
    bench("ext-name handoff: String::clone", iters, || {
        let s = black_box(&name_string).clone();
        black_box(s.len());
    });
    let name_arc: std::sync::Arc<str> = std::sync::Arc::from("postgres_scanner");
    bench("ext-name handoff: Arc<str>::clone", iters, || {
        let s = std::sync::Arc::clone(black_box(&name_arc));
        black_box(s.len());
    });

    println!();

    // --- 2. value marshalling (every arg in + result out, per row) -----------
    use ducklink_runtime::duckdb_extension_bindings::duckdb::extension::types as t;
    let prim = t::Duckvalue::Int64(black_box(42));
    bench("marshal primitive (Int64 round-trip)", iters, || {
        let v = clone_duckvalue(black_box(&prim));
        black_box(v);
    });

    let text = t::Duckvalue::Text("a representative scalar argument".to_string());
    bench("marshal text (owned String move/clone)", iters / 5, || {
        let v = clone_duckvalue(black_box(&text));
        black_box(v);
    });

    let complex = t::Duckvalue::Complex(t::Complexvalue {
        type_expr: "STRUCT(a INTEGER, b VARCHAR)".to_string(),
        json: r#"{"a":1,"b":"x"}"#.to_string(),
    });
    bench("marshal complex (nested via Complex arm)", iters / 5, || {
        let v = clone_duckvalue(black_box(&complex));
        black_box(v);
    });

    // --- 3. expanded marshalling: the rich @3.x value arms -------------------
    // The @3.1.0 surface widened `duckvalue` from the v1 closed set to 22 arms.
    // These are the per-cell marshalling costs the new arms add (all fixed-width,
    // so all in the cheap tier alongside Int64 -- confirming the rich types do not
    // regress the hot path the way Text/Complex do).
    let decimal = t::Duckvalue::Decimal(t::Decimalvalue {
        lower: 0x0123_4567_89ab_cdef,
        upper: 0,
        width: 38,
        scale: 4,
    });
    bench("marshal decimal (i128 fixed-width)", iters, || {
        black_box(clone_duckvalue(black_box(&decimal)));
    });
    let uuid = t::Duckvalue::Uuid(t::Uuidvalue { hi: 1, lo: 2 });
    bench("marshal uuid (2x u64 fixed-width)", iters, || {
        black_box(clone_duckvalue(black_box(&uuid)));
    });
    let interval = t::Duckvalue::Interval(t::Intervalvalue {
        months: 1,
        days: 2,
        micros: 3,
    });
    bench("marshal interval (fixed-width struct)", iters, || {
        black_box(clone_duckvalue(black_box(&interval)));
    });
    let ts = t::Duckvalue::Timestamp(black_box(1_700_000_000_000_000));
    bench("marshal timestamp (i64 fixed-width)", iters, || {
        black_box(clone_duckvalue(black_box(&ts)));
    });

    println!();

    // NOTE on the wasm-core scalar read hoist: `read_scalar_argument` now reads
    // a per-column cached data + validity pointer (fetched ONCE per column in
    // `execute_scalar_function`) and decodes the validity bitmap inline, instead
    // of calling `duckdb_vector_get_data` + `duckdb_vector_get_validity` +
    // `duckdb_validity_row_is_valid` (three FFI/boundary calls) per CELL. That
    // win is the elimination of ~3 * rows * cols cross-module wasm calls per
    // chunk; it is NOT representable on native (a native call to a trivial fn is
    // ~1ns and swamped by noise), so it is deliberately NOT benched here -- it
    // must be measured in-wasm (wasi-sdk build). The benches below isolate the
    // costs that ARE native-measurable: per-row allocation, marshalling, and the
    // window re-send.
    const ROWS: usize = 2048;

    // --- 4. aggregate / window rowbatch marshalling (new @3.1.0 dispatch) ----
    // call-aggregate hands the WHOLE buffered group across once; call-aggregate-
    // window hands the WHOLE partition across PER OUTPUT ROW (the frozen frame
    // ABI). These bench the marshalling each pays so the window re-send cost is
    // quantified (informs whether a 3.2.0 "send partition once" interface earns
    // its keep).
    let group: Vec<Vec<t::Duckvalue>> =
        (0..ROWS as i64).map(|i| vec![t::Duckvalue::Int64(i)]).collect();
    bench("aggregate: marshal 2048-row group once", 20_000, || {
        let cloned: Vec<Vec<t::Duckvalue>> =
            black_box(&group).iter().map(|r| r.clone()).collect();
        black_box(cloned);
    });

    // window: a 256-row partition re-marshalled once PER output row = 256 sends.
    const PART: usize = 256;
    let partition: Vec<Vec<t::Duckvalue>> =
        (0..PART as i64).map(|i| vec![t::Duckvalue::Int64(i)]).collect();
    bench("window: re-marshal 256-part PER row x256", 2_000, || {
        for _out_row in 0..PART {
            let sent: Vec<Vec<t::Duckvalue>> =
                black_box(&partition).iter().map(|r| r.clone()).collect();
            black_box(sent);
        }
    });
    bench("window: marshal 256-part ONCE (3.2.0 hypo)", 2_000, || {
        let sent: Vec<Vec<t::Duckvalue>> =
            black_box(&partition).iter().map(|r| r.clone()).collect();
        for _out_row in 0..PART {
            black_box(&sent);
        }
    });
}

/// Mirrors the structurally-identical enum reconstruction the host runs in
/// `convert_core_duckvalue_to_extension` / `convert_extension_duckvalue_to_core`
/// (here as a clone since both sides are the same generated type in this crate).
fn clone_duckvalue(
    v: &ducklink_runtime::duckdb_extension_bindings::duckdb::extension::types::Duckvalue,
) -> ducklink_runtime::duckdb_extension_bindings::duckdb::extension::types::Duckvalue {
    v.clone()
}
