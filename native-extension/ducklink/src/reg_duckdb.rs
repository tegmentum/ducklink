//! The Direction-2 DuckDB sink: register the scalar functions a wasm component
//! declared as real DuckDB scalar functions, dispatching each call back into the
//! component via [`crate::engine::Engine2`].
//!
//! MVP slice: unary `BIGINT -> BIGINT` scalars. The public duckdb-rs
//! registration path (`register_scalar_function_with_state`) derives the SQL
//! signature from a static `VScalar::signatures()`, so one `VScalar` impl serves
//! one fixed shape; the per-function callback handle is injected through the
//! function's `State`. Other shapes are skipped with a logged note — extending
//! to them is more `VScalar` impls keyed by `reg::LogicalType`.

use std::cell::RefCell;
use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use duckdb::core::{DataChunkHandle, FlatVector, Inserter, LogicalTypeHandle, LogicalTypeId};
use duckdb::ffi::duckdb_string_t;
use duckdb::types::DuckString;
use duckdb::vscalar::{ScalarFunctionSignature, VScalar};
use duckdb::vtab::arrow::WritableVector;
use duckdb::vtab::{BindInfo, InitInfo, TableFunctionInfo, VTab, Value};
use duckdb::Connection;

use ducklink_runtime::reg;

use crate::engine::{Engine2, ScalarFunc, TableFunc};

/// Per-function state DuckDB hands back to `invoke`: which component callback to
/// dispatch to, the shared engine, and the function's argument / return type
/// codes (so one `WasmScalar` serves every signature).
#[derive(Clone)]
struct WasmScalarState {
    callback_handle: u32,
    engine: Arc<Mutex<Engine2>>,
    arg_codes: Vec<u8>,
    ret_code: u8,
}

// Bridge type codes — one per DuckDB logical type the scalar bridge marshals.
const T_I64: u8 = 0;
const T_U64: u8 = 1;
const T_F64: u8 = 2;
const T_BOOL: u8 = 3;
const T_TEXT: u8 = 4;
const T_BLOB: u8 = 5;

/// Map a neutral logical type to a bridge type code. All current `reg`
/// logical types are supported.
fn type_code(lt: reg::LogicalType) -> u8 {
    match lt {
        reg::LogicalType::Int64 => T_I64,
        reg::LogicalType::Uint64 => T_U64,
        reg::LogicalType::Float64 => T_F64,
        reg::LogicalType::Boolean => T_BOOL,
        reg::LogicalType::Text => T_TEXT,
        reg::LogicalType::Blob => T_BLOB,
    }
}

fn logical_type(code: u8) -> LogicalTypeHandle {
    let id = match code {
        T_I64 => LogicalTypeId::Bigint,
        T_U64 => LogicalTypeId::UBigint,
        T_F64 => LogicalTypeId::Double,
        T_BOOL => LogicalTypeId::Boolean,
        T_TEXT => LogicalTypeId::Varchar,
        T_BLOB => LogicalTypeId::Blob,
        _ => unreachable!("type code out of range"),
    };
    LogicalTypeHandle::from(id)
}

/// Read row `i` of a flat input column (type `code`) into a neutral value.
fn read_arg(code: u8, vec: &FlatVector, i: usize, len: usize) -> reg::DuckValue {
    match code {
        T_I64 => reg::DuckValue::Int64(unsafe { vec.as_slice_with_len::<i64>(len) }[i]),
        T_U64 => reg::DuckValue::Uint64(unsafe { vec.as_slice_with_len::<u64>(len) }[i]),
        T_F64 => reg::DuckValue::Float64(unsafe { vec.as_slice_with_len::<f64>(len) }[i]),
        T_BOOL => reg::DuckValue::Boolean(unsafe { vec.as_slice_with_len::<bool>(len) }[i]),
        T_TEXT => {
            let mut s = unsafe { vec.as_slice_with_len::<duckdb_string_t>(len) }[i];
            reg::DuckValue::Text(DuckString::new(&mut s).as_str().into_owned())
        }
        T_BLOB => {
            let mut s = unsafe { vec.as_slice_with_len::<duckdb_string_t>(len) }[i];
            reg::DuckValue::Blob(DuckString::new(&mut s).as_bytes().to_vec())
        }
        _ => unreachable!("type code out of range"),
    }
}

/// Write a neutral value into row `i` of a flat output column (type `code`).
fn write_ret(
    code: u8,
    vec: &mut FlatVector,
    i: usize,
    len: usize,
    v: reg::DuckValue,
) -> Result<(), Box<dyn std::error::Error>> {
    match (code, v) {
        (T_I64, reg::DuckValue::Int64(x)) => {
            let s = unsafe { vec.as_mut_slice_with_len::<i64>(len) };
            s[i] = x;
        }
        (T_U64, reg::DuckValue::Uint64(x)) => {
            let s = unsafe { vec.as_mut_slice_with_len::<u64>(len) };
            s[i] = x;
        }
        (T_F64, reg::DuckValue::Float64(x)) => {
            let s = unsafe { vec.as_mut_slice_with_len::<f64>(len) };
            s[i] = x;
        }
        (T_BOOL, reg::DuckValue::Boolean(x)) => {
            let s = unsafe { vec.as_mut_slice_with_len::<bool>(len) };
            s[i] = x;
        }
        (T_TEXT, reg::DuckValue::Text(x)) => vec.insert(i, x.as_str()),
        (T_BLOB, reg::DuckValue::Blob(x)) => vec.insert(i, x.as_slice()),
        (_, other) => {
            return Err(format!(
                "component returned {other:?}, incompatible with declared return type"
            )
            .into());
        }
    }
    Ok(())
}

// The signature for the next `register_scalar_function_with_state` call.
// `VScalar::signatures()` is a static method with no access to the function's
// state, so the per-function signature is handed to it through this thread-local,
// set immediately before the (synchronous) registration call.
thread_local! {
    static PENDING_SIGNATURE: RefCell<Option<(Vec<u8>, u8)>> = const { RefCell::new(None) };
}

/// One `VScalar` impl serving every component scalar. The argument / return
/// types come from the state (for dispatch) and from `PENDING_SIGNATURE` (for
/// the SQL signature), so any arity and any supported type combination works.
struct WasmScalar;

impl VScalar for WasmScalar {
    type State = WasmScalarState;

    fn invoke(
        state: &Self::State,
        input: &mut DataChunkHandle,
        output: &mut dyn WritableVector,
    ) -> Result<(), Box<dyn std::error::Error>> {
        let len = input.len();
        let cols: Vec<FlatVector> = (0..state.arg_codes.len())
            .map(|j| input.flat_vector(j))
            .collect();
        let mut out = output.flat_vector();

        let mut engine = state.engine.lock().expect("engine mutex poisoned");
        for i in 0..len {
            let args: Vec<reg::DuckValue> = state
                .arg_codes
                .iter()
                .enumerate()
                .map(|(j, &code)| read_arg(code, &cols[j], i, len))
                .collect();
            let result = engine
                .dispatch_scalar(state.callback_handle, i as u64, args)
                .map_err(|e| -> Box<dyn std::error::Error> { e.to_string().into() })?;
            write_ret(state.ret_code, &mut out, i, len, result)?;
        }
        Ok(())
    }

    fn signatures() -> Vec<ScalarFunctionSignature> {
        let (arg_codes, ret_code) = PENDING_SIGNATURE
            .with(|s| s.borrow().clone())
            .expect("PENDING_SIGNATURE must be set before registration");
        vec![ScalarFunctionSignature::exact(
            arg_codes.into_iter().map(logical_type).collect(),
            logical_type(ret_code),
        )]
    }
}

/// Register every component scalar on `con`. Returns the count registered. All
/// `reg` logical types are supported across any arity.
pub fn register_scalars(
    con: &Connection,
    engine: Arc<Mutex<Engine2>>,
    scalars: &[ScalarFunc],
) -> duckdb::Result<usize> {
    let mut registered = 0usize;
    for f in scalars {
        let arg_codes: Vec<u8> = f.arguments.iter().map(|a| type_code(a.logical)).collect();
        let ret_code = type_code(f.returns);
        let state = WasmScalarState {
            callback_handle: f.callback_handle,
            engine: engine.clone(),
            arg_codes: arg_codes.clone(),
            ret_code,
        };
        // Hand the signature to `WasmScalar::signatures()` for this one call.
        PENDING_SIGNATURE.with(|s| *s.borrow_mut() = Some((arg_codes, ret_code)));
        let result = con.register_scalar_function_with_state::<WasmScalar>(&f.name, &state);
        PENDING_SIGNATURE.with(|s| *s.borrow_mut() = None);
        result?;
        registered += 1;
    }
    Ok(registered)
}

// ---------------------------------------------------------------------------
// Table functions
// ---------------------------------------------------------------------------

/// Convert a DuckDB call-parameter value (from `BindInfo::get_parameter`) into a
/// neutral value, extracting it as the function's declared argument type `code`.
fn param_to_neutral(code: u8, v: &Value) -> reg::DuckValue {
    if v.is_null() {
        return reg::DuckValue::Null;
    }
    match code {
        T_I64 => reg::DuckValue::Int64(v.to_int64()),
        T_U64 => reg::DuckValue::Uint64(v.to_uint64()),
        T_F64 => reg::DuckValue::Float64(v.to_double()),
        T_BOOL => reg::DuckValue::Boolean(v.to_bool()),
        T_TEXT => reg::DuckValue::Text(v.to_string()),
        // No raw blob getter on the param value; fall back to its text form.
        T_BLOB => reg::DuckValue::Blob(v.to_string().into_bytes()),
        _ => reg::DuckValue::Null,
    }
}

/// Per-function table data, passed to the static `VTab` callbacks via DuckDB's
/// extra-info slot.
#[derive(Clone)]
struct WasmTableExtra {
    callback_handle: u32,
    engine: Arc<Mutex<Engine2>>,
    arg_codes: Vec<u8>,
    col_codes: Vec<u8>,
    col_names: Vec<String>,
}

/// Bind result: the full set of rows the component produced for this call, plus
/// the column type codes used to write them out.
struct WasmTableBind {
    rows: Vec<Vec<reg::DuckValue>>,
    col_codes: Vec<u8>,
}

/// Init state: a cursor over `WasmTableBind::rows` across `func` chunks.
struct WasmTableInit {
    cursor: AtomicUsize,
}

// The parameter types for the next table-function registration — handed to the
// static `VTab::parameters()` the same way `PENDING_SIGNATURE` feeds scalars.
thread_local! {
    static PENDING_TABLE_PARAMS: RefCell<Option<Vec<u8>>> = const { RefCell::new(None) };
}

/// One `VTab` impl serving every component table function. `bind` runs the
/// component once with the call parameters and buffers all rows; `func` streams
/// them back in DuckDB vector-sized chunks.
struct WasmTable;

impl VTab for WasmTable {
    type InitData = WasmTableInit;
    type BindData = WasmTableBind;

    fn bind(bind: &BindInfo) -> Result<Self::BindData, Box<dyn std::error::Error>> {
        let extra = unsafe { &*bind.get_extra_info::<WasmTableExtra>() };
        for (name, &code) in extra.col_names.iter().zip(&extra.col_codes) {
            bind.add_result_column(name, logical_type(code));
        }
        let args: Vec<reg::DuckValue> = extra
            .arg_codes
            .iter()
            .enumerate()
            .map(|(j, &code)| param_to_neutral(code, &bind.get_parameter(j as u64)))
            .collect();
        let rows = {
            let mut engine = extra.engine.lock().expect("engine mutex poisoned");
            engine
                .dispatch_table(extra.callback_handle, args)
                .map_err(|e| -> Box<dyn std::error::Error> { e.to_string().into() })?
        };
        Ok(WasmTableBind {
            rows,
            col_codes: extra.col_codes.clone(),
        })
    }

    fn init(_: &InitInfo) -> Result<Self::InitData, Box<dyn std::error::Error>> {
        Ok(WasmTableInit {
            cursor: AtomicUsize::new(0),
        })
    }

    fn func(
        func: &TableFunctionInfo<Self>,
        output: &mut DataChunkHandle,
    ) -> Result<(), Box<dyn std::error::Error>> {
        let bind = func.get_bind_data();
        let init = func.get_init_data();
        let start = init.cursor.load(Ordering::Relaxed);
        let n = bind.rows.len().saturating_sub(start).min(2048);
        if n == 0 {
            output.set_len(0);
            return Ok(());
        }
        for (c, &code) in bind.col_codes.iter().enumerate() {
            let mut col = output.flat_vector(c);
            for r in 0..n {
                let val = bind.rows[start + r][c].clone();
                write_ret(code, &mut col, r, n, val)?;
            }
        }
        init.cursor.store(start + n, Ordering::Relaxed);
        output.set_len(n);
        Ok(())
    }

    fn parameters() -> Option<Vec<LogicalTypeHandle>> {
        PENDING_TABLE_PARAMS
            .with(|s| s.borrow().clone())
            .map(|codes| codes.into_iter().map(logical_type).collect())
    }
}

/// Register every component table function on `con`. Returns the count
/// registered. Parameter and column types use the same `reg` logical-type set as
/// scalars.
pub fn register_tables(
    con: &Connection,
    engine: Arc<Mutex<Engine2>>,
    tables: &[TableFunc],
) -> duckdb::Result<usize> {
    let mut registered = 0usize;
    for t in tables {
        let arg_codes: Vec<u8> = t.arguments.iter().map(|a| type_code(a.logical)).collect();
        let col_codes: Vec<u8> = t.columns.iter().map(|c| type_code(c.logical)).collect();
        let col_names: Vec<String> = t.columns.iter().map(|c| c.name.clone()).collect();
        let extra = WasmTableExtra {
            callback_handle: t.callback_handle,
            engine: engine.clone(),
            arg_codes: arg_codes.clone(),
            col_codes,
            col_names,
        };
        PENDING_TABLE_PARAMS.with(|s| *s.borrow_mut() = Some(arg_codes));
        let result =
            con.register_table_function_with_extra_info::<WasmTable, WasmTableExtra>(&t.name, &extra);
        PENDING_TABLE_PARAMS.with(|s| *s.borrow_mut() = None);
        result?;
        registered += 1;
    }
    Ok(registered)
}

/// A component to load at extension-load time: a display name and a path to the
/// `.wasm` artifact.
#[derive(Clone, Debug)]
pub struct ComponentSpec {
    pub name: String,
    pub path: PathBuf,
}

/// Parse the `DUCKLINK_COMPONENTS` environment variable into specs. The value is
/// a `:`-separated list; each entry is either `name=path` or a bare `path`
/// (whose file stem becomes the name). Empty / unset yields no specs.
///
/// This is how a deployment selects which components `LOAD ducklink` exposes —
/// catalog registration is a load-time operation, so components are named up
/// front rather than via an in-query `CALL`.
pub fn component_specs_from_env() -> Vec<ComponentSpec> {
    let raw = std::env::var("DUCKLINK_COMPONENTS").unwrap_or_default();
    raw.split(':')
        .filter(|entry| !entry.is_empty())
        .map(|entry| match entry.split_once('=') {
            Some((name, path)) => ComponentSpec {
                name: name.to_string(),
                path: PathBuf::from(path),
            },
            None => {
                let path = PathBuf::from(entry);
                let name = path
                    .file_stem()
                    .and_then(|s| s.to_str())
                    .unwrap_or("component")
                    .to_string();
                ComponentSpec { name, path }
            }
        })
        .collect()
}

/// Load each component and register its scalar functions on `con`, sharing one
/// `engine`. Returns the total number of scalar functions registered. The
/// `engine` `Arc` is cloned into every registered function's state, so the loaded
/// components stay alive as long as the functions remain in the catalog.
pub fn register_components(
    con: &Connection,
    engine: Arc<Mutex<Engine2>>,
    specs: &[ComponentSpec],
) -> anyhow::Result<usize> {
    let mut total = 0usize;
    for spec in specs {
        let loaded = {
            let mut e = engine.lock().expect("engine mutex poisoned");
            e.load(&spec.name, &spec.path)?
        };
        total += register_scalars(con, engine.clone(), &loaded.scalars)?;
        total += register_tables(con, engine.clone(), &loaded.tables)?;
    }
    Ok(total)
}

#[cfg(all(test, feature = "bundled"))]
mod tests {
    use super::*;
    use crate::engine::Engine2;
    use std::path::PathBuf;

    fn sample_component() -> PathBuf {
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../artifacts/extensions/sample_extension.wasm")
    }

    /// End-to-end: load the sample wasm component, register its
    /// `sample_plus_one(BIGINT)->BIGINT` scalar into a real in-process DuckDB,
    /// and confirm the +1 is computed inside the wasm component.
    #[test]
    fn sample_plus_one_dispatches_into_wasm() {
        let mut engine = Engine2::new().expect("engine");
        let loaded = engine
            .load("sample_extension", &sample_component())
            .expect("load component");
        let engine = Arc::new(Mutex::new(engine));

        let con = Connection::open_in_memory().expect("open duckdb");
        let n = register_scalars(&con, engine.clone(), &loaded.scalars).expect("register");
        assert!(n >= 1, "expected at least one BIGINT->BIGINT scalar, got {n}");

        let v: i64 = con
            .query_row("SELECT sample_plus_one(41)", [], |r| r.get(0))
            .expect("query");
        assert_eq!(v, 42, "sample_plus_one(41) should be 42 (computed in wasm)");

        // A batch, to exercise the per-row dispatch loop.
        let sum: i64 = con
            .query_row(
                "SELECT sum(sample_plus_one(i)) FROM range(5) t(i)",
                [],
                |r| r.get(0),
            )
            .expect("query batch");
        assert_eq!(sum, 1 + 2 + 3 + 4 + 5, "sum of (i+1) for i in 0..5");
    }

    /// `register_components` — the path the loadable entry point takes — loads a
    /// component by spec and registers its scalars.
    #[test]
    fn register_components_exposes_scalar() {
        let engine = Arc::new(Mutex::new(Engine2::new().expect("engine")));
        let specs = vec![ComponentSpec {
            name: "sample_extension".to_string(),
            path: sample_component(),
        }];
        let con = Connection::open_in_memory().expect("open duckdb");
        let n = register_components(&con, engine, &specs).expect("register components");
        assert!(n >= 1, "expected >=1 scalar registered, got {n}");

        let v: i64 = con
            .query_row("SELECT sample_plus_one(7)", [], |r| r.get(0))
            .expect("query");
        assert_eq!(v, 8);
    }

    /// End-to-end table function: `sample_emit_sequence(limit)` emits rows
    /// `0..limit` from inside the wasm component, streamed back through the VTab
    /// bridge.
    #[test]
    fn sample_emit_sequence_streams_from_wasm() {
        let engine = Arc::new(Mutex::new(Engine2::new().expect("engine")));
        let specs = vec![ComponentSpec {
            name: "sample_extension".to_string(),
            path: sample_component(),
        }];
        let con = Connection::open_in_memory().expect("open duckdb");
        register_components(&con, engine, &specs).expect("register components");

        let count: i64 = con
            .query_row("SELECT count(*) FROM sample_emit_sequence(5)", [], |r| {
                r.get(0)
            })
            .expect("count query");
        assert_eq!(count, 5, "sample_emit_sequence(5) emits 5 rows");

        let sum: i64 = con
            .query_row(
                "SELECT sum(value) FROM sample_emit_sequence(5)",
                [],
                |r| r.get(0),
            )
            .expect("sum query");
        assert_eq!(sum, 0 + 1 + 2 + 3 + 4, "sum of values 0..5");
    }

    #[test]
    fn env_specs_parse_name_and_bare_path() {
        // Safety: single-threaded within this test; no other test reads the var.
        unsafe {
            std::env::set_var("DUCKLINK_COMPONENTS", "sample=/a/b.wasm:/c/isin.wasm");
        }
        let specs = component_specs_from_env();
        unsafe {
            std::env::remove_var("DUCKLINK_COMPONENTS");
        }
        assert_eq!(specs.len(), 2);
        assert_eq!(specs[0].name, "sample");
        assert_eq!(specs[0].path, PathBuf::from("/a/b.wasm"));
        assert_eq!(specs[1].name, "isin", "bare path -> file stem as name");
        assert_eq!(specs[1].path, PathBuf::from("/c/isin.wasm"));
    }
}
