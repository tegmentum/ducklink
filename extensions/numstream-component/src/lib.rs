//! numstream — a STREAMING + FILTER-PUSHDOWN-capable table function, the
//! end-to-end proof for the v3 freeze policy's first additive MINOR (3.1.0).
//!
//!   numstream(n)  ->  one BIGINT column `v` with the rows v = 0, 1, ..., n-1
//!
//! It OPTS IN to filter pushdown by declaring itself through the additive 3.1.0
//! `table-stream` registration interface (NOT the frozen `runtime.table-registry`)
//! and exporting `table-stream-dispatch`. When the engine pushes a conjunctive
//! filter set down to the scan (e.g. `WHERE v > 5`), the host hands it to
//! `call-table-open-filtered` as a neutral, by-value-safe descriptor (column index
//! + comparator + constant) and THIS component prunes the generated rows AT THE
//! SOURCE — so a row that fails the pushed filter is never produced.
//!
//! Freeze-compliance: only an opt-in component like this rebuilds at @3.1.0; every
//! existing @3.0.0 component keeps loading un-rebuilt because the shared
//! types/runtime enums were not touched.

use std::collections::HashMap;
use std::sync::{
    atomic::{AtomicU32, Ordering},
    Mutex, OnceLock,
};

use wit_bindgen::rt::string::String;
use wit_bindgen::rt::vec::Vec;

wit_bindgen::generate!({
    path: "./wit",
    world: "duckdb:extension/duckdb-extension-table-stream",
});

use duckdb::extension::{table_stream, types};
use exports::duckdb::extension::{callback_dispatch, guest, table_stream_dispatch};

use table_stream_dispatch::{FilterOp, TableFilter, TableOpenResult};
use types::{Columndef, Duckerror, Duckvalue, Funcarg, Logicaltype, Resultset};

/// The callback-handle the host threads back into every streaming dispatch call.
const HANDLE: u32 = 1;

struct Extension;

// ---------------------------------------------------------------------------
// Per-cursor scan state, keyed by an opaque cursor id handed back from open.
// ---------------------------------------------------------------------------
struct Cursor {
    /// Total rows the function would emit unfiltered (the `n` argument).
    n: i64,
    /// Next candidate value to test/emit.
    next: i64,
    /// Conjunctive (AND) pushed-down filters on the emitted column `v`.
    filters: Vec<TableFilter>,
}

fn cursors() -> &'static Mutex<HashMap<u32, Cursor>> {
    static C: OnceLock<Mutex<HashMap<u32, Cursor>>> = OnceLock::new();
    C.get_or_init(|| Mutex::new(HashMap::new()))
}

fn next_cursor_id() -> u32 {
    static N: AtomicU32 = AtomicU32::new(1);
    N.fetch_add(1, Ordering::Relaxed)
}

/// The single emitted column: `v BIGINT`.
fn out_columns() -> Vec<Columndef> {
    let mut v = Vec::new();
    v.push(Columndef {
        name: "v".into(),
        logical: Logicaltype::Int64,
    });
    v
}

// ---------------------------------------------------------------------------
// load(): declare the filterable streaming table function via the 3.1.0 marker.
// ---------------------------------------------------------------------------
impl guest::Guest for Extension {
    fn load() -> Result<types::Loadresult, Duckerror> {
        // arg: n BIGINT
        let mut args = Vec::new();
        args.push(Funcarg {
            name: Some("n".into()),
            logical: Logicaltype::Int64,
        });
        table_stream::register_filterable_table("numstream", &args, &out_columns(), HANDLE)?;
        Ok(types::Loadresult {
            name: "numstream".into(),
            version: Some(env!("CARGO_PKG_VERSION").into()),
            requires: Vec::new().into(),
        })
    }
    fn reconfigure(_keys: Vec<String>) -> Result<bool, Duckerror> {
        Ok(false)
    }
    fn shutdown() -> Result<bool, Duckerror> {
        Ok(false)
    }
}

// ---------------------------------------------------------------------------
// Filter application — the component honors the pushed-down predicate, so a row
// that fails it is pruned at the source (never produced).
// ---------------------------------------------------------------------------
fn const_i64(values: &[Duckvalue]) -> Option<i64> {
    match values.first() {
        Some(Duckvalue::Int64(x)) => Some(*x),
        Some(Duckvalue::Int32(x)) => Some(*x as i64),
        Some(Duckvalue::Uint64(x)) => Some(*x as i64),
        Some(Duckvalue::Uint32(x)) => Some(*x as i64),
        _ => None,
    }
}

fn passes_filter(v: i64, f: &TableFilter) -> bool {
    // Single emitted column `v` at position 0; ignore any other column index.
    if f.column != 0 {
        return true;
    }
    match f.op {
        FilterOp::IsNull => false,     // v is never NULL
        FilterOp::IsNotNull => true,   // v is always NOT NULL
        FilterOp::IsIn => {
            f.values.iter().any(|val| match val {
                Duckvalue::Int64(x) => *x == v,
                Duckvalue::Int32(x) => *x as i64 == v,
                _ => false,
            })
        }
        other => {
            let Some(c) = const_i64(&f.values) else {
                return true; // unshippable constant -> don't prune (stay correct)
            };
            match other {
                FilterOp::Eq => v == c,
                FilterOp::Ne => v != c,
                FilterOp::Lt => v < c,
                FilterOp::Le => v <= c,
                FilterOp::Gt => v > c,
                FilterOp::Ge => v >= c,
                _ => true,
            }
        }
    }
}

fn passes_all(v: i64, filters: &[TableFilter]) -> bool {
    filters.iter().all(|f| passes_filter(v, f))
}

// ---------------------------------------------------------------------------
// table-stream-dispatch: the open/next/close cursor, with the filtered variant.
// ---------------------------------------------------------------------------
fn parse_n(args: &[Duckvalue]) -> Result<i64, Duckerror> {
    match args.first() {
        Some(Duckvalue::Int64(n)) => Ok(*n),
        Some(Duckvalue::Int32(n)) => Ok(*n as i64),
        Some(Duckvalue::Uint64(n)) => Ok(*n as i64),
        Some(Duckvalue::Uint32(n)) => Ok(*n as i64),
        _ => Err(Duckerror::Invalidargument(
            "numstream(n): expected a single integer argument".into(),
        )),
    }
}

fn open_inner(args: &[Duckvalue], filters: Vec<TableFilter>) -> Result<TableOpenResult, Duckerror> {
    let n = parse_n(args)?;
    let id = next_cursor_id();
    if !filters.is_empty() {
        eprintln!(
            "[numstream] open-filtered: n={n}, {} pushed-down filter(s) received -> pruning at source",
            filters.len()
        );
    }
    cursors()
        .lock()
        .unwrap()
        .insert(id, Cursor { n, next: 0, filters });
    Ok(TableOpenResult {
        cursor: id,
        columns: out_columns(),
    })
}

impl table_stream_dispatch::Guest for Extension {
    fn call_table_open(
        _handle: u32,
        args: Vec<Duckvalue>,
        _projection: Vec<u32>,
    ) -> Result<TableOpenResult, Duckerror> {
        open_inner(&args, Vec::new())
    }

    fn call_table_open_filtered(
        _handle: u32,
        args: Vec<Duckvalue>,
        _projection: Vec<u32>,
        filters: Vec<TableFilter>,
    ) -> Result<TableOpenResult, Duckerror> {
        open_inner(&args, filters)
    }

    fn call_table_next(
        _handle: u32,
        cursor: u32,
        max_rows: u32,
    ) -> Result<Resultset, Duckerror> {
        let mut guard = cursors().lock().unwrap();
        let cur = guard
            .get_mut(&cursor)
            .ok_or_else(|| Duckerror::Internal("numstream: unknown cursor".into()))?;
        let mut rows: std::vec::Vec<std::vec::Vec<Duckvalue>> = std::vec::Vec::new();
        while cur.next < cur.n && (rows.len() as u32) < max_rows {
            let v = cur.next;
            cur.next += 1;
            if passes_all(v, &cur.filters) {
                rows.push(vec![Duckvalue::Int64(v)]);
            }
        }
        Ok(rows.into())
    }

    fn call_table_close(_handle: u32, cursor: u32) -> Result<bool, Duckerror> {
        Ok(cursors().lock().unwrap().remove(&cursor).is_some())
    }
}

// ---------------------------------------------------------------------------
// callback-dispatch is mandated by the world but unused here (this component has
// no scalar/whole-batch-table/aggregate/pragma/cast callbacks).
// ---------------------------------------------------------------------------
impl callback_dispatch::Guest for Extension {
    // major-4 columnar dispatch: numstream streams via table-stream-dispatch and
    // has no scalar/aggregate/cast, so the three columnar hot methods are stubs.
    datalink_extcore::columnar_stub!();

    fn call_scalar(
        _handle: u32,
        _args: Vec<Duckvalue>,
        _ctx: types::Invokeinfo,
    ) -> Result<Duckvalue, Duckerror> {
        Err(Duckerror::Unsupported("numstream: no scalar fns".into()))
    }
    fn call_table(_handle: u32, _args: Vec<Duckvalue>) -> Result<Resultset, Duckerror> {
        Err(Duckerror::Unsupported(
            "numstream: streams via table-stream-dispatch".into(),
        ))
    }
    fn call_pragma(
        _handle: u32,
        _args: Vec<Duckvalue>,
    ) -> Result<Option<Duckvalue>, Duckerror> {
        Err(Duckerror::Unsupported("numstream: no pragmas".into()))
    }
    fn call_cast(_handle: u32, _value: Duckvalue) -> Result<Duckvalue, Duckerror> {
        Err(Duckerror::Unsupported("numstream: no casts".into()))
    }
}

export!(Extension);
