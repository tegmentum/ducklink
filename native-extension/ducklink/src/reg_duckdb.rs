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

use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use duckdb::core::{DataChunkHandle, FlatVector, LogicalTypeHandle, LogicalTypeId};
use duckdb::vscalar::{ScalarFunctionSignature, VScalar};
use duckdb::vtab::arrow::WritableVector;
use duckdb::Connection;

use ducklink_runtime::reg;

use crate::engine::{Engine2, ScalarFunc};

/// Per-function state DuckDB hands back to `invoke`: which component callback to
/// dispatch to, and the shared engine that owns the loaded component.
#[derive(Clone)]
struct WasmScalarState {
    callback_handle: u32,
    engine: Arc<Mutex<Engine2>>,
}

// Type codes for the const-generic scalar bridge. Each (arg, return) pair is a
// distinct `WasmScalar<A, R>` monomorphization, because duckdb-rs derives the SQL
// signature from a static `VScalar::signatures()`.
const T_I64: u8 = 0;
const T_U64: u8 = 1;
const T_F64: u8 = 2;
const T_BOOL: u8 = 3;

/// Map a neutral logical type to a bridge type code, or `None` if unsupported
/// (Text/Blob — those need string-vector marshalling, a follow-up).
fn type_code(lt: reg::LogicalType) -> Option<u8> {
    match lt {
        reg::LogicalType::Int64 => Some(T_I64),
        reg::LogicalType::Uint64 => Some(T_U64),
        reg::LogicalType::Float64 => Some(T_F64),
        reg::LogicalType::Boolean => Some(T_BOOL),
        reg::LogicalType::Text | reg::LogicalType::Blob => None,
    }
}

fn logical_type(code: u8) -> LogicalTypeHandle {
    let id = match code {
        T_I64 => LogicalTypeId::Bigint,
        T_U64 => LogicalTypeId::UBigint,
        T_F64 => LogicalTypeId::Double,
        T_BOOL => LogicalTypeId::Boolean,
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
        (_, other) => {
            return Err(format!(
                "component returned {other:?}, incompatible with declared return type"
            )
            .into());
        }
    }
    Ok(())
}

/// A unary numeric/bool scalar backed by a wasm component function. `A` is the
/// argument type code, `R` the return type code (see `T_*`). Each invocation
/// dispatches every row through `Engine2::dispatch_scalar` into the component.
struct WasmScalar<const A: u8, const R: u8>;

impl<const A: u8, const R: u8> VScalar for WasmScalar<A, R> {
    type State = WasmScalarState;

    fn invoke(
        state: &Self::State,
        input: &mut DataChunkHandle,
        output: &mut dyn WritableVector,
    ) -> Result<(), Box<dyn std::error::Error>> {
        let len = input.len();
        let in_vec = input.flat_vector(0);
        let mut out_vec = output.flat_vector();

        let mut engine = state.engine.lock().expect("engine mutex poisoned");
        for i in 0..len {
            let arg = read_arg(A, &in_vec, i, len);
            let result = engine
                .dispatch_scalar(state.callback_handle, i as u64, vec![arg])
                .map_err(|e| -> Box<dyn std::error::Error> { e.to_string().into() })?;
            write_ret(R, &mut out_vec, i, len, result)?;
        }
        Ok(())
    }

    fn signatures() -> Vec<ScalarFunctionSignature> {
        vec![ScalarFunctionSignature::exact(
            vec![logical_type(A)],
            logical_type(R),
        )]
    }
}

/// The (arg, return) type codes for `f`, if it is a unary scalar over the
/// numeric/bool types this bridge covers; `None` otherwise.
fn supported_codes(f: &ScalarFunc) -> Option<(u8, u8)> {
    if f.arguments.len() != 1 {
        return None;
    }
    Some((type_code(f.arguments[0].logical)?, type_code(f.returns)?))
}

/// Register one scalar, dispatching to the `WasmScalar<A, R>` monomorphization
/// for the given type codes.
fn register_one(
    con: &Connection,
    name: &str,
    state: &WasmScalarState,
    a: u8,
    r: u8,
) -> duckdb::Result<()> {
    macro_rules! go {
        ($A:literal, $R:literal) => {
            con.register_scalar_function_with_state::<WasmScalar<$A, $R>>(name, state)
        };
    }
    match (a, r) {
        (0, 0) => go!(0, 0),
        (0, 1) => go!(0, 1),
        (0, 2) => go!(0, 2),
        (0, 3) => go!(0, 3),
        (1, 0) => go!(1, 0),
        (1, 1) => go!(1, 1),
        (1, 2) => go!(1, 2),
        (1, 3) => go!(1, 3),
        (2, 0) => go!(2, 0),
        (2, 1) => go!(2, 1),
        (2, 2) => go!(2, 2),
        (2, 3) => go!(2, 3),
        (3, 0) => go!(3, 0),
        (3, 1) => go!(3, 1),
        (3, 2) => go!(3, 2),
        (3, 3) => go!(3, 3),
        _ => unreachable!("type codes are 0..4"),
    }
}

/// Register every supported component scalar on `con`. Returns the count
/// registered; unsupported shapes (multi-arg, or Text/Blob) are skipped with a
/// logged note. The bridge covers unary INT64/UINT64/DOUBLE/BOOLEAN -> any of
/// the same.
pub fn register_scalars(
    con: &Connection,
    engine: Arc<Mutex<Engine2>>,
    scalars: &[ScalarFunc],
) -> duckdb::Result<usize> {
    let mut registered = 0usize;
    for f in scalars {
        let (a, r) = match supported_codes(f) {
            Some(codes) => codes,
            None => {
                eprintln!(
                    "[ducklink] skipping scalar '{}': unsupported signature (bridge covers unary INT64/UINT64/DOUBLE/BOOLEAN)",
                    f.name
                );
                continue;
            }
        };
        let state = WasmScalarState {
            callback_handle: f.callback_handle,
            engine: engine.clone(),
        };
        register_one(con, &f.name, &state, a, r)?;
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
        let scalars = {
            let mut e = engine.lock().expect("engine mutex poisoned");
            e.load(&spec.name, &spec.path)?
        };
        total += register_scalars(con, engine.clone(), &scalars)?;
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
        let scalars = engine
            .load("sample_extension", &sample_component())
            .expect("load component");
        let engine = Arc::new(Mutex::new(engine));

        let con = Connection::open_in_memory().expect("open duckdb");
        let n = register_scalars(&con, engine.clone(), &scalars).expect("register");
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
