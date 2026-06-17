use std::collections::HashMap;
use std::sync::{
    atomic::{AtomicU32, Ordering},
    Mutex, OnceLock,
};

use wit_bindgen::rt::string::String;
use wit_bindgen::rt::vec::Vec;

wit_bindgen::generate!({
    path: "./wit",
    world: "duckdb:extension/duckdb-extension",
});

use duckdb::extension::{catalog, files, runtime, types};
use exports::duckdb::extension::{callback_dispatch, guest};

struct SampleExtension;

impl guest::Guest for SampleExtension {
    fn load() -> Result<types::Loadresult, types::Duckerror> {
        register_scalar_function()?;
        register_table_function()?;
        register_aggregate_function()?;
        register_macro_definition()?;
        register_logical_type_definition()?;
        register_cast_definition()?;
        register_replacement_scan()?;
        Ok(types::Loadresult {
            name: "sample_extension".into(),
            version: Some(env!("CARGO_PKG_VERSION").into()),
            requires: Vec::new().into(),
        })
    }

    fn reconfigure(_keys: Vec<String>) -> Result<bool, types::Duckerror> {
        Ok(false)
    }

    fn shutdown() -> Result<bool, types::Duckerror> {
        Ok(false)
    }
}

impl callback_dispatch::Guest for SampleExtension {
    fn call_scalar(
        handle: u32,
        args: Vec<types::Duckvalue>,
        _ctx: types::Invokeinfo,
    ) -> Result<types::Duckvalue, types::Duckerror> {
        let handler = scalar_handlers()
            .lock()
            .expect("scalar handler mutex poisoned")
            .get(&handle)
            .cloned()
            .ok_or_else(|| types::Duckerror::Internal("unknown scalar handle".into()))?;

        match handler {
            ScalarHandler::AddOne => {
                let value = match args.as_slice() {
                    [types::Duckvalue::Int64(v)] => *v,
                    _ => {
                        return Err(types::Duckerror::Invalidargument(
                            "sample_plus_one expects a single INT64 argument".into(),
                        ))
                    }
                };
                Ok(types::Duckvalue::Int64(value + 1))
            }
        }
    }

    fn call_table(
        handle: u32,
        args: Vec<types::Duckvalue>,
    ) -> Result<types::Resultset, types::Duckerror> {
        let handler = table_handlers()
            .lock()
            .expect("table handler mutex poisoned")
            .get(&handle)
            .copied()
            .ok_or_else(|| types::Duckerror::Internal("unknown table handle".into()))?;

        match handler {
            TableHandler::EmitSequence => {
                let limit = match args.as_slice() {
                    [types::Duckvalue::Int64(v)] => *v,
                    _ => {
                        return Err(types::Duckerror::Invalidargument(
                            "sample_emit_sequence expects a single INT64 argument".into(),
                        ))
                    }
                };
                if limit < 0 {
                    return Err(types::Duckerror::Invalidargument(
                        "sample_emit_sequence expects a non-negative argument".into(),
                    ));
                }
                let mut rows = Vec::with_capacity(limit as usize);
                for value in 0..limit {
                    rows.push(vec![types::Duckvalue::Int64(value)]);
                }
                Ok(rows)
            }
            TableHandler::ReadPath => {
                // Reached via the replacement scan: `FROM 'file.sample'` rewrites
                // to `sample_read_path('file.sample')`. Echo the path back as one
                // row (a real extension would open and read the file here).
                let path = match args.as_slice() {
                    [types::Duckvalue::Text(path)] => path.clone(),
                    _ => {
                        return Err(types::Duckerror::Invalidargument(
                            "sample_read_path expects a single VARCHAR argument".into(),
                        ))
                    }
                };
                Ok(vec![vec![types::Duckvalue::Text(path)]])
            }
        }
    }

    fn call_aggregate(
        handle: u32,
        rows: types::Rowbatch,
    ) -> Result<types::Duckvalue, types::Duckerror> {
        let handler = aggregate_handlers()
            .lock()
            .expect("aggregate handler mutex poisoned")
            .get(&handle)
            .copied()
            .ok_or_else(|| types::Duckerror::Internal("unknown aggregate handle".into()))?;

        match handler {
            AggregateHandler::SumIntegers => {
                let mut total: i64 = 0;
                for row in rows {
                    match row.first() {
                        Some(types::Duckvalue::Int64(value)) => {
                            total = total.saturating_add(*value);
                        }
                        Some(types::Duckvalue::Null) | None => {}
                        other => {
                            return Err(types::Duckerror::Invalidargument(format!(
                                "sample_sum expects INT64 input, saw {other:?}"
                            )));
                        }
                    }
                }
                Ok(types::Duckvalue::Int64(total))
            }
        }
    }

    fn call_pragma(
        _handle: u32,
        _args: Vec<types::Duckvalue>,
    ) -> Result<Option<types::Duckvalue>, types::Duckerror> {
        Err(types::Duckerror::Unsupported(
            "pragma callbacks not implemented in sample extension".into(),
        ))
    }

    fn call_cast(handle: u32, value: types::Duckvalue) -> Result<types::Duckvalue, types::Duckerror> {
        let handler = cast_handlers()
            .lock()
            .expect("cast handler mutex poisoned")
            .get(&handle)
            .cloned()
            .ok_or_else(|| types::Duckerror::Internal("unknown cast handle".into()))?;

        match handler {
            CastHandler::ParseId => match value {
                types::Duckvalue::Null => Ok(types::Duckvalue::Null),
                types::Duckvalue::Text(text) => {
                    let digits = text.strip_prefix("id-").unwrap_or(&text);
                    let parsed = digits.parse::<i64>().map_err(|_| {
                        types::Duckerror::Invalidargument(format!(
                            "cannot cast '{text}' to sample_id (expected id-<n>)"
                        ))
                    })?;
                    Ok(types::Duckvalue::Int64(parsed))
                }
                other => Err(types::Duckerror::Invalidargument(format!(
                    "sample_id cast expects VARCHAR, saw {other:?}"
                ))),
            },
        }
    }
}

export!(SampleExtension);

/// Registers a SQL macro via the `catalog` interface. Unlike the scalar/table/
/// aggregate callbacks, a macro is pure SQL — the host forwards it to DuckDB as
/// `CREATE MACRO`.
fn register_macro_definition() -> Result<(), types::Duckerror> {
    catalog::register_macro(&catalog::MacroDef {
        schema: String::new(),
        name: "sample_add_two".into(),
        parameters: vec!["x".into()],
        definition_sql: "x + 2".into(),
    })
    .map_err(types::Duckerror::Internal)
}

/// Registers a custom cast `VARCHAR -> sample_id` that parses "id-<n>" into
/// the integer <n>. The built-in VARCHAR->integer cast would fail on "id-7", so
/// a successful result proves the custom cast ran.
fn register_cast_definition() -> Result<(), types::Duckerror> {
    let handle = NEXT_CAST_HANDLE.fetch_add(1, Ordering::Relaxed);
    cast_handlers()
        .lock()
        .expect("cast handler mutex poisoned")
        .insert(handle, CastHandler::ParseId);

    let callback = runtime::CastCallback::new(handle);
    catalog::register_cast(
        &catalog::CastSpec {
            from: "VARCHAR".into(),
            to: "sample_id".into(),
            kind: catalog::CastKind::Explicit,
        },
        callback,
    )
    .map_err(types::Duckerror::Internal)
}

/// Registers a named SQL type alias via the `catalog` interface. The host
/// forwards it to DuckDB as `CREATE TYPE`.
fn register_logical_type_definition() -> Result<(), types::Duckerror> {
    catalog::register_logical_type(&catalog::LogicalType {
        name: "sample_id".into(),
        physical: "BIGINT".into(),
    })
    .map(|_| ())
    .map_err(types::Duckerror::Internal)
}

/// Registers a `sample_read_path(VARCHAR)` table function and a replacement scan
/// so that `SELECT * FROM 'anything.sample'` rewrites to
/// `sample_read_path('anything.sample')`.
fn register_replacement_scan() -> Result<(), types::Duckerror> {
    let capability = runtime::get_capability(types::Capabilitykind::Table)
        .ok_or_else(|| types::Duckerror::Internal("host did not expose table capability".into()))?;
    let registry = match capability {
        runtime::Capability::Table(registry) => registry,
        _ => {
            return Err(types::Duckerror::Internal(
                "table capability returned unexpected variant".into(),
            ))
        }
    };

    let handle = NEXT_TABLE_HANDLE.fetch_add(1, Ordering::Relaxed);
    table_handlers()
        .lock()
        .expect("table handler mutex poisoned")
        .insert(handle, TableHandler::ReadPath);

    let callback = runtime::TableCallback::new(handle);
    let args = vec![runtime::Funcarg {
        name: Some("path".into()),
        logical: types::Logicaltype::Text,
    }];
    let columns = vec![types::Columndef {
        name: "path".into(),
        logical: types::Logicaltype::Text,
    }];
    let opts = runtime::Extopts {
        description: Some("Returns the path it was given (replacement-scan demo)".into()),
        tags: vec!["sample".into()],
    };
    let table_function = registry.register("sample_read_path", &args, &columns, callback, Some(&opts))?;

    files::register_replacement_scan(&files::ReplacementScan {
        extensions: vec!["sample".into()],
        table_function,
        mode: files::DetectionMode::ExtensionOnly,
    })
    .map_err(types::Duckerror::Internal)?;
    Ok(())
}

fn register_scalar_function() -> Result<(), types::Duckerror> {
    let capability = runtime::get_capability(types::Capabilitykind::Scalar).ok_or_else(|| {
        types::Duckerror::Internal("host did not expose scalar capability".into())
    })?;

    let registry = match capability {
        runtime::Capability::Scalar(registry) => registry,
        _ => {
            return Err(types::Duckerror::Internal(
                "scalar capability returned unexpected variant".into(),
            ))
        }
    };

    let handle = NEXT_SCALAR_HANDLE.fetch_add(1, Ordering::Relaxed);
    scalar_handlers()
        .lock()
        .expect("scalar handler mutex poisoned")
        .insert(handle, ScalarHandler::AddOne);

    let callback = runtime::ScalarCallback::new(handle);
    let args = vec![runtime::Funcarg {
        name: Some("value".into()),
        logical: types::Logicaltype::Int64,
    }];
    let opts = runtime::Funcopts {
        description: Some("Adds one to the input integer".into()),
        tags: vec!["sample".into()],
        attributes: types::Funcflags::DETERMINISTIC | types::Funcflags::STATELESS,
    };

    registry.register(
        "sample_plus_one",
        &args,
        types::Logicaltype::Int64,
        callback,
        Some(&opts),
    )?;
    Ok(())
}

fn register_table_function() -> Result<(), types::Duckerror> {
    let capability = runtime::get_capability(types::Capabilitykind::Table)
        .ok_or_else(|| types::Duckerror::Internal("host did not expose table capability".into()))?;

    let registry = match capability {
        runtime::Capability::Table(registry) => registry,
        _ => {
            return Err(types::Duckerror::Internal(
                "table capability returned unexpected variant".into(),
            ))
        }
    };

    let handle = NEXT_TABLE_HANDLE.fetch_add(1, Ordering::Relaxed);
    table_handlers()
        .lock()
        .expect("table handler mutex poisoned")
        .insert(handle, TableHandler::EmitSequence);

    let callback = runtime::TableCallback::new(handle);
    let args = vec![runtime::Funcarg {
        name: Some("limit".into()),
        logical: types::Logicaltype::Int64,
    }];
    let columns = vec![types::Columndef {
        name: "value".into(),
        logical: types::Logicaltype::Int64,
    }];
    let opts = runtime::Extopts {
        description: Some("Emits integers from 0 up to the provided limit".into()),
        tags: vec!["sample".into()],
    };

    registry.register(
        "sample_emit_sequence",
        &args,
        &columns,
        callback,
        Some(&opts),
    )?;
    Ok(())
}

fn register_aggregate_function() -> Result<(), types::Duckerror> {
    let capability =
        runtime::get_capability(types::Capabilitykind::Aggregate).ok_or_else(|| {
            types::Duckerror::Internal("host did not expose aggregate capability".into())
        })?;

    let registry = match capability {
        runtime::Capability::Aggregate(registry) => registry,
        _ => {
            return Err(types::Duckerror::Internal(
                "aggregate capability returned unexpected variant".into(),
            ))
        }
    };

    let handle = NEXT_AGGREGATE_HANDLE.fetch_add(1, Ordering::Relaxed);
    aggregate_handlers()
        .lock()
        .expect("aggregate handler mutex poisoned")
        .insert(handle, AggregateHandler::SumIntegers);

    let callback = runtime::AggregateCallback::new(handle);
    let args = vec![runtime::Funcarg {
        name: Some("value".into()),
        logical: types::Logicaltype::Int64,
    }];
    let opts = runtime::Funcopts {
        description: Some("Sums INT64 inputs provided to the aggregate".into()),
        tags: vec!["sample".into()],
        attributes: types::Funcflags::DETERMINISTIC | types::Funcflags::STATELESS,
    };

    registry.register(
        "sample_sum",
        &args,
        types::Logicaltype::Int64,
        callback,
        Some(&opts),
    )?;
    Ok(())
}

#[derive(Clone, Copy)]
enum ScalarHandler {
    AddOne,
}

#[derive(Clone, Copy)]
enum TableHandler {
    EmitSequence,
    ReadPath,
}

#[derive(Clone, Copy)]
enum AggregateHandler {
    SumIntegers,
}

#[derive(Clone, Copy)]
enum CastHandler {
    /// Parses "id-<n>" VARCHAR into the integer <n>.
    ParseId,
}

static NEXT_SCALAR_HANDLE: AtomicU32 = AtomicU32::new(1);
static SCALAR_HANDLERS: OnceLock<Mutex<HashMap<u32, ScalarHandler>>> = OnceLock::new();
static NEXT_TABLE_HANDLE: AtomicU32 = AtomicU32::new(1);
static TABLE_HANDLERS: OnceLock<Mutex<HashMap<u32, TableHandler>>> = OnceLock::new();
static NEXT_AGGREGATE_HANDLE: AtomicU32 = AtomicU32::new(1);
static AGGREGATE_HANDLERS: OnceLock<Mutex<HashMap<u32, AggregateHandler>>> = OnceLock::new();
static NEXT_CAST_HANDLE: AtomicU32 = AtomicU32::new(1);
static CAST_HANDLERS: OnceLock<Mutex<HashMap<u32, CastHandler>>> = OnceLock::new();

fn cast_handlers() -> &'static Mutex<HashMap<u32, CastHandler>> {
    CAST_HANDLERS.get_or_init(|| Mutex::new(HashMap::new()))
}

fn scalar_handlers() -> &'static Mutex<HashMap<u32, ScalarHandler>> {
    SCALAR_HANDLERS.get_or_init(|| Mutex::new(HashMap::new()))
}

fn table_handlers() -> &'static Mutex<HashMap<u32, TableHandler>> {
    TABLE_HANDLERS.get_or_init(|| Mutex::new(HashMap::new()))
}

fn aggregate_handlers() -> &'static Mutex<HashMap<u32, AggregateHandler>> {
    AGGREGATE_HANDLERS.get_or_init(|| Mutex::new(HashMap::new()))
}
