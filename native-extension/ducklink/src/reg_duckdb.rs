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

use duckdb::core::{DataChunkHandle, LogicalTypeHandle, LogicalTypeId};
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

/// A unary `BIGINT -> BIGINT` scalar backed by a wasm component function.
struct WasmScalarI64;

impl VScalar for WasmScalarI64 {
    type State = WasmScalarState;

    fn invoke(
        state: &Self::State,
        input: &mut DataChunkHandle,
        output: &mut dyn WritableVector,
    ) -> Result<(), Box<dyn std::error::Error>> {
        let len = input.len();
        let src = input.flat_vector(0);
        let src = unsafe { src.as_slice_with_len::<i64>(len) };
        let mut out = output.flat_vector();
        let dst = unsafe { out.as_mut_slice_with_len::<i64>(len) };

        let mut engine = state.engine.lock().expect("engine mutex poisoned");
        for (i, (d, s)) in dst.iter_mut().zip(src).enumerate() {
            let result = engine
                .dispatch_scalar(state.callback_handle, i as u64, vec![reg::DuckValue::Int64(*s)])
                .map_err(|e| -> Box<dyn std::error::Error> { e.to_string().into() })?;
            *d = match result {
                reg::DuckValue::Int64(v) => v,
                other => {
                    return Err(format!("expected Int64 result, got {other:?}").into());
                }
            };
        }
        Ok(())
    }

    fn signatures() -> Vec<ScalarFunctionSignature> {
        vec![ScalarFunctionSignature::exact(
            vec![LogicalTypeHandle::from(LogicalTypeId::Bigint)],
            LogicalTypeHandle::from(LogicalTypeId::Bigint),
        )]
    }
}

/// Returns true if `f` is the shape this MVP can bridge (unary BIGINT->BIGINT).
fn is_i64_unary(f: &ScalarFunc) -> bool {
    f.arguments.len() == 1
        && matches!(f.arguments[0].logical, reg::LogicalType::Int64)
        && matches!(f.returns, reg::LogicalType::Int64)
}

/// Register every supported component scalar on `con`. Returns the count
/// registered; unsupported shapes are skipped with a logged note.
pub fn register_scalars(
    con: &Connection,
    engine: Arc<Mutex<Engine2>>,
    scalars: &[ScalarFunc],
) -> duckdb::Result<usize> {
    let mut registered = 0usize;
    for f in scalars {
        if !is_i64_unary(f) {
            eprintln!(
                "[ducklink] skipping scalar '{}': MVP supports only unary BIGINT->BIGINT",
                f.name
            );
            continue;
        }
        let state = WasmScalarState {
            callback_handle: f.callback_handle,
            engine: engine.clone(),
        };
        con.register_scalar_function_with_state::<WasmScalarI64>(&f.name, &state)?;
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
