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

use crate::engine::{AggregateFunc, Engine2, ScalarFunc, TableFunc};

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
        // A component may return SQL NULL for any declared return type (e.g. a
        // validator on bad input) — mark the output row invalid.
        (_, reg::DuckValue::Null) => vec.set_null(i),
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

        // Marshal the whole chunk, then cross into the component once. DuckDB
        // hands us a chunk of up to STANDARD_VECTOR_SIZE rows; dispatching each
        // row individually pays a WIT boundary crossing per row, which dominates
        // for cheap scalars. Build every row's argument tuple up front and call
        // the batched dispatcher a single time.
        let rows: Vec<Vec<reg::DuckValue>> = (0..len)
            .map(|i| {
                state
                    .arg_codes
                    .iter()
                    .enumerate()
                    .map(|(j, &code)| read_arg(code, &cols[j], i, len))
                    .collect()
            })
            .collect();

        let mut engine = state.engine.lock().expect("engine mutex poisoned");
        let results = engine
            .dispatch_scalar_batch(state.callback_handle, 0, rows)
            .map_err(|e| -> Box<dyn std::error::Error> { e.to_string().into() })?;
        drop(engine);

        if results.len() != len {
            return Err(format!(
                "scalar (callback {}) returned {} results for {} input rows",
                state.callback_handle,
                results.len(),
                len
            )
            .into());
        }
        for (i, result) in results.into_iter().enumerate() {
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

// ---------------------------------------------------------------------------
// Aggregate functions (raw C API)
// ---------------------------------------------------------------------------
//
// duckdb-rs has no safe aggregate wrapper, so aggregates go through the raw C
// API. DuckDB's aggregate is incremental (init/update/combine/finalize); the
// wasm component computes over ALL rows at once, so each group's state simply
// accumulates its input rows and `finalize` hands them to the component.

use std::ffi::CString;
use std::os::raw::{c_char, c_void};

use duckdb::ffi;

/// Per-group aggregate state: the input rows accumulated for this group, each a
/// tuple of the function's argument values.
type AggState = Vec<Vec<reg::DuckValue>>;

/// Per-function data DuckDB hands to the aggregate callbacks via extra-info.
struct AggExtra {
    callback_handle: u32,
    engine: Arc<Mutex<Engine2>>,
    arg_codes: Vec<u8>,
    ret_code: u8,
}

fn duckdb_type_of(code: u8) -> ffi::duckdb_type {
    match code {
        T_I64 => ffi::DUCKDB_TYPE_DUCKDB_TYPE_BIGINT,
        T_U64 => ffi::DUCKDB_TYPE_DUCKDB_TYPE_UBIGINT,
        T_F64 => ffi::DUCKDB_TYPE_DUCKDB_TYPE_DOUBLE,
        T_BOOL => ffi::DUCKDB_TYPE_DUCKDB_TYPE_BOOLEAN,
        T_TEXT => ffi::DUCKDB_TYPE_DUCKDB_TYPE_VARCHAR,
        T_BLOB => ffi::DUCKDB_TYPE_DUCKDB_TYPE_BLOB,
        _ => ffi::DUCKDB_TYPE_DUCKDB_TYPE_VARCHAR,
    }
}

/// Read row `i` of a raw input vector (type `code`) into a neutral value.
unsafe fn read_arg_raw(code: u8, vector: ffi::duckdb_vector, i: usize) -> reg::DuckValue {
    let validity = ffi::duckdb_vector_get_validity(vector);
    if !validity.is_null() && !ffi::duckdb_validity_row_is_valid(validity, i as u64) {
        return reg::DuckValue::Null;
    }
    let data = ffi::duckdb_vector_get_data(vector);
    match code {
        T_I64 => reg::DuckValue::Int64(*(data as *const i64).add(i)),
        T_U64 => reg::DuckValue::Uint64(*(data as *const u64).add(i)),
        T_F64 => reg::DuckValue::Float64(*(data as *const f64).add(i)),
        T_BOOL => reg::DuckValue::Boolean(*(data as *const bool).add(i)),
        T_TEXT => {
            let mut s = *(data as *const duckdb_string_t).add(i);
            reg::DuckValue::Text(DuckString::new(&mut s).as_str().into_owned())
        }
        T_BLOB => {
            let mut s = *(data as *const duckdb_string_t).add(i);
            reg::DuckValue::Blob(DuckString::new(&mut s).as_bytes().to_vec())
        }
        _ => reg::DuckValue::Null,
    }
}

/// Write a neutral value into row `i` of a raw result vector (type `code`).
unsafe fn write_ret_raw(
    code: u8,
    vector: ffi::duckdb_vector,
    i: usize,
    v: reg::DuckValue,
) -> Result<(), String> {
    if matches!(v, reg::DuckValue::Null) {
        ffi::duckdb_vector_ensure_validity_writable(vector);
        let validity = ffi::duckdb_vector_get_validity(vector);
        ffi::duckdb_validity_set_row_validity(validity, i as u64, false);
        return Ok(());
    }
    let data = ffi::duckdb_vector_get_data(vector);
    match (code, v) {
        (T_I64, reg::DuckValue::Int64(x)) => *(data as *mut i64).add(i) = x,
        (T_U64, reg::DuckValue::Uint64(x)) => *(data as *mut u64).add(i) = x,
        (T_F64, reg::DuckValue::Float64(x)) => *(data as *mut f64).add(i) = x,
        (T_BOOL, reg::DuckValue::Boolean(x)) => *(data as *mut bool).add(i) = x,
        (T_TEXT, reg::DuckValue::Text(s)) => {
            ffi::duckdb_vector_assign_string_element_len(
                vector,
                i as u64,
                s.as_ptr() as *const c_char,
                s.len() as u64,
            );
        }
        (T_BLOB, reg::DuckValue::Blob(b)) => {
            ffi::duckdb_vector_assign_string_element_len(
                vector,
                i as u64,
                b.as_ptr() as *const c_char,
                b.len() as u64,
            );
        }
        (_, other) => {
            return Err(format!(
                "component returned {other:?}, incompatible with declared aggregate return type"
            ));
        }
    }
    Ok(())
}

unsafe extern "C" fn agg_state_size(_info: ffi::duckdb_function_info) -> ffi::idx_t {
    std::mem::size_of::<*mut AggState>() as ffi::idx_t
}

unsafe extern "C" fn agg_init(_info: ffi::duckdb_function_info, state: ffi::duckdb_aggregate_state) {
    let slot = state as *mut *mut AggState;
    *slot = Box::into_raw(Box::new(AggState::new()));
}

unsafe extern "C" fn agg_update(
    info: ffi::duckdb_function_info,
    input: ffi::duckdb_data_chunk,
    states: *mut ffi::duckdb_aggregate_state,
) {
    let extra = &*(ffi::duckdb_aggregate_function_get_extra_info(info) as *const AggExtra);
    let n = ffi::duckdb_data_chunk_get_size(input) as usize;
    let ncols = extra.arg_codes.len();
    let vectors: Vec<ffi::duckdb_vector> = (0..ncols)
        .map(|c| ffi::duckdb_data_chunk_get_vector(input, c as u64))
        .collect();
    for row in 0..n {
        // The state for this input row (states is parallel to the input chunk).
        let st = *states.add(row);
        let group = &mut **(st as *mut *mut AggState);
        let argrow: Vec<reg::DuckValue> = (0..ncols)
            .map(|c| read_arg_raw(extra.arg_codes[c], vectors[c], row))
            .collect();
        group.push(argrow);
    }
}

unsafe extern "C" fn agg_combine(
    _info: ffi::duckdb_function_info,
    source: *mut ffi::duckdb_aggregate_state,
    target: *mut ffi::duckdb_aggregate_state,
    count: ffi::idx_t,
) {
    for i in 0..count as usize {
        let s = &mut **(*source.add(i) as *mut *mut AggState);
        let t = &mut **(*target.add(i) as *mut *mut AggState);
        t.append(s);
    }
}

unsafe extern "C" fn agg_finalize(
    info: ffi::duckdb_function_info,
    source: *mut ffi::duckdb_aggregate_state,
    result: ffi::duckdb_vector,
    count: ffi::idx_t,
    offset: ffi::idx_t,
) {
    let extra = &*(ffi::duckdb_aggregate_function_get_extra_info(info) as *const AggExtra);
    for i in 0..count as usize {
        let group = &mut **(*source.add(i) as *mut *mut AggState);
        let rows = std::mem::take(group);
        let dispatched = {
            let mut engine = extra.engine.lock().expect("engine mutex poisoned");
            engine.dispatch_aggregate(extra.callback_handle, rows)
        };
        let out = offset as usize + i;
        let write = dispatched
            .map_err(|e| e.to_string())
            .and_then(|v| write_ret_raw(extra.ret_code, result, out, v));
        if let Err(msg) = write {
            if let Ok(c) = CString::new(msg) {
                ffi::duckdb_aggregate_function_set_error(info, c.as_ptr());
            }
            return;
        }
    }
}

unsafe extern "C" fn agg_destroy(states: *mut ffi::duckdb_aggregate_state, count: ffi::idx_t) {
    for i in 0..count as usize {
        let p = *(*states.add(i) as *mut *mut AggState);
        if !p.is_null() {
            drop(Box::from_raw(p));
        }
    }
}

unsafe extern "C" fn agg_extra_destroy(ptr: *mut c_void) {
    if !ptr.is_null() {
        drop(Box::from_raw(ptr as *mut AggExtra));
    }
}

/// Register every component aggregate function on the raw connection `raw_con`
/// via the DuckDB C API. Functions registered on any connection of a database
/// are visible to all its connections, so `raw_con` need only share the database
/// with the connection used for queries.
///
/// # Safety
/// `raw_con` must be a valid `duckdb_connection`.
pub unsafe fn register_aggregates(
    raw_con: ffi::duckdb_connection,
    engine: Arc<Mutex<Engine2>>,
    aggregates: &[AggregateFunc],
) -> duckdb::Result<usize> {
    let mut registered = 0usize;
    for f in aggregates {
        let arg_codes: Vec<u8> = f.arguments.iter().map(|a| type_code(a.logical)).collect();
        let ret_code = type_code(f.returns);

        let func = ffi::duckdb_create_aggregate_function();
        let cname = CString::new(f.name.as_str())
            .map_err(|_| duckdb::Error::DuckDBFailure(ffi::Error::new(ffi::DuckDBError), None))?;
        ffi::duckdb_aggregate_function_set_name(func, cname.as_ptr());
        for &code in &arg_codes {
            let mut lt = ffi::duckdb_create_logical_type(duckdb_type_of(code));
            ffi::duckdb_aggregate_function_add_parameter(func, lt);
            ffi::duckdb_destroy_logical_type(&mut lt);
        }
        let mut rlt = ffi::duckdb_create_logical_type(duckdb_type_of(ret_code));
        ffi::duckdb_aggregate_function_set_return_type(func, rlt);
        ffi::duckdb_destroy_logical_type(&mut rlt);

        let extra = Box::into_raw(Box::new(AggExtra {
            callback_handle: f.callback_handle,
            engine: engine.clone(),
            arg_codes,
            ret_code,
        })) as *mut c_void;
        ffi::duckdb_aggregate_function_set_extra_info(func, extra, Some(agg_extra_destroy));
        ffi::duckdb_aggregate_function_set_functions(
            func,
            Some(agg_state_size),
            Some(agg_init),
            Some(agg_update),
            Some(agg_combine),
            Some(agg_finalize),
        );
        ffi::duckdb_aggregate_function_set_destructor(func, Some(agg_destroy));

        let rc = ffi::duckdb_register_aggregate_function(raw_con, func);
        let mut func_mut = func;
        ffi::duckdb_destroy_aggregate_function(&mut func_mut);
        if rc != ffi::DuckDBSuccess {
            return Err(duckdb::Error::DuckDBFailure(
                ffi::Error::new(ffi::DuckDBError),
                Some(format!("failed to register aggregate '{}'", f.name)),
            ));
        }
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

/// Load each component and register its functions, sharing one `engine`. Scalars
/// and table functions register on the duckdb-rs `con`; aggregates (which need
/// the raw C API) register on `raw_con` when supplied — pass `None` (e.g. the
/// loadable entry point, which has no raw connection) to skip them with a note.
/// Returns the total number of functions registered. The `engine` `Arc` is
/// cloned into every registered function's state, so the loaded components stay
/// alive as long as the functions remain in the catalog.
///
/// # Safety
/// When `Some`, `raw_con` must be a valid `duckdb_connection` sharing the
/// database with `con` (so the aggregates it registers are visible to `con`).
pub fn register_components(
    con: &Connection,
    raw_con: Option<ffi::duckdb_connection>,
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
        match raw_con {
            Some(rc) => {
                total += unsafe { register_aggregates(rc, engine.clone(), &loaded.aggregates)? };
            }
            None if !loaded.aggregates.is_empty() => {
                eprintln!(
                    "[ducklink] skipping {} aggregate function(s) from '{}': no raw connection available",
                    loaded.aggregates.len(),
                    spec.name
                );
            }
            None => {}
        }
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
        let n = register_components(&con, None, engine, &specs).expect("register components");
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
        register_components(&con, None, engine, &specs).expect("register components");

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

    /// Feasibility probe for the aggregate bridge: are functions registered via
    /// the C API visible across connections of the same database? (Aggregate
    /// registration needs a raw `duckdb_connection`, which the duckdb-rs
    /// `Connection` doesn't expose — so we'd register on a separate connection of
    /// the same db and rely on db-wide visibility.)
    #[test]
    fn registered_function_visible_across_connections() {
        let mut engine = Engine2::new().expect("engine");
        let loaded = engine
            .load("sample_extension", &sample_component())
            .expect("load");
        let engine = Arc::new(Mutex::new(engine));
        let con = Connection::open_in_memory().expect("open");
        register_scalars(&con, engine, &loaded.scalars).expect("register");

        // sample_plus_one is registered on `con`; query it from a clone.
        let con2 = con.try_clone().expect("clone connection");
        let v: i64 = con2
            .query_row("SELECT sample_plus_one(41)", [], |r| r.get(0))
            .expect("query on cloned connection");
        assert_eq!(v, 42, "C-API function should be visible on a sibling connection");
    }
}
