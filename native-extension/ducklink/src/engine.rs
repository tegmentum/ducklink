//! The Direction-2 engine: loads `duckdb:extension` WebAssembly components into
//! native DuckDB and dispatches DuckDB invocations back into them.
//!
//! This module depends ONLY on `ducklink-runtime` + wasmtime (no DuckDB), so it
//! compiles and is checkable without the DuckDB toolchain. The DuckDB C-API
//! binding that turns a [`ScalarFunc`] into a registered catalog function (and
//! routes per-row calls back to [`Engine2::dispatch_scalar`]) lives behind the
//! crate's `loadable` feature.

use std::collections::HashMap;
use std::path::Path;
use std::sync::{Arc, Mutex};

use anyhow::{anyhow, Context, Result};
use wasmtime::component::Component;
use wasmtime::{Config, Engine};
use wasmtime_wasi::{WasiCtx, WasiCtxBuilder};

use ducklink_runtime::duckdb_extension_bindings::duckdb::extension::{
    runtime as extension_runtime, types as extension_types,
};
use ducklink_runtime::reg;
use ducklink_runtime::{
    load_component, CallbackRegistry, ConfigError, ExtensionInstance, ExtensionServices, LogField,
    LogLevel, PendingRegistrationsData,
};

/// Build a component-model wasmtime engine for running extension components.
/// Mirrors the host's engine config (component model + wasm exceptions, which
/// DuckDB-targeting components may use).
fn build_engine() -> Result<Engine> {
    let mut config = Config::new();
    config.wasm_component_model(true);
    config.wasm_exceptions(true);
    // Cache compiled artifacts on disk. Cranelift-compiling a component dominates
    // load time; with the cache a repeated load of the same component deserializes
    // in ~ms instead of recompiling. Fall back silently if the cache is
    // unavailable (e.g. no writable home dir) — it is a pure performance hint.
    match wasmtime::Cache::from_file(None) {
        Ok(cache) => {
            config.cache(Some(cache));
        }
        Err(err) => {
            eprintln!("[ducklink] wasmtime compile cache unavailable: {err}");
        }
    }
    Engine::new(&config).context("failed to create wasmtime engine")
}

/// Config/logging sink for native DuckDB. Logging goes to stderr; config reads
/// are not yet wired to DuckDB's settings (they return `None`). Routing these to
/// the DuckDB C API is a follow-up; components that only register functions do
/// not depend on it.
struct NativeServices;

impl ExtensionServices for NativeServices {
    fn provider_version(&mut self) -> Result<String, ConfigError> {
        Ok(concat!("ducklink-extension/", env!("CARGO_PKG_VERSION")).to_string())
    }
    fn list_keys(&mut self, _prefix: Option<&str>) -> Result<Vec<String>, ConfigError> {
        Ok(Vec::new())
    }
    fn get_string(&mut self, _path: &str) -> Result<Option<String>, ConfigError> {
        Ok(None)
    }
    fn get_bool(&mut self, _path: &str) -> Result<Option<bool>, ConfigError> {
        Ok(None)
    }
    fn get_i64(&mut self, _path: &str) -> Result<Option<i64>, ConfigError> {
        Ok(None)
    }
    fn get_u64(&mut self, _path: &str) -> Result<Option<u64>, ConfigError> {
        Ok(None)
    }
    fn get_f64(&mut self, _path: &str) -> Result<Option<f64>, ConfigError> {
        Ok(None)
    }
    fn get_bytes(&mut self, _path: &str) -> Result<Option<Vec<u8>>, ConfigError> {
        Ok(None)
    }
    fn get_string_list(&mut self, _path: &str) -> Result<Option<Vec<String>>, ConfigError> {
        Ok(None)
    }
    fn log(&mut self, level: LogLevel, message: &str, target: Option<&str>) {
        match target {
            Some(t) => eprintln!("[ducklink:{level:?}:{t}] {message}"),
            None => eprintln!("[ducklink:{level:?}] {message}"),
        }
    }
    fn log_fields(&mut self, level: LogLevel, message: &str, fields: &[LogField]) {
        let rendered: Vec<String> = fields
            .iter()
            .map(|f| format!("{}={}", f.key, f.value))
            .collect();
        eprintln!("[ducklink:{level:?}] {message} {{{}}}", rendered.join(", "));
    }
}

/// A scalar function a loaded component registered, ready to bridge into
/// DuckDB's catalog. `callback_handle` routes back through the engine's callback
/// registry to the owning component on each invocation.
#[derive(Clone, Debug)]
pub struct ScalarFunc {
    pub extension: String,
    pub name: String,
    pub arguments: Vec<reg::FuncArg>,
    pub returns: reg::LogicalType,
    pub callback_handle: u32,
}

/// A table function a loaded component registered. `arguments` are the call
/// parameters; `columns` are the result schema; `callback_handle` dispatches.
#[derive(Clone, Debug)]
pub struct TableFunc {
    pub extension: String,
    pub name: String,
    pub arguments: Vec<reg::FuncArg>,
    pub columns: Vec<reg::ColumnDef>,
    pub callback_handle: u32,
}

/// An aggregate function a loaded component registered. `arguments` are the
/// input columns; the component computes over all rows at finalize.
#[derive(Clone, Debug)]
pub struct AggregateFunc {
    pub extension: String,
    pub name: String,
    pub arguments: Vec<reg::FuncArg>,
    pub returns: reg::LogicalType,
    pub callback_handle: u32,
}

/// What a component registered: the functions a direction-specific sink bridges
/// into the database.
#[derive(Clone, Debug, Default)]
pub struct LoadedComponent {
    pub scalars: Vec<ScalarFunc>,
    pub tables: Vec<TableFunc>,
    pub aggregates: Vec<AggregateFunc>,
}

/// Process-wide Direction-2 engine: loads components and dispatches DuckDB
/// invocations into them. A DuckDB extension holds one of these.
pub struct Engine2 {
    engine: Engine,
    callbacks: Arc<Mutex<CallbackRegistry>>,
    instances: HashMap<String, ExtensionInstance>,
}

impl Engine2 {
    pub fn new() -> Result<Self> {
        Ok(Self {
            engine: build_engine()?,
            callbacks: Arc::new(Mutex::new(CallbackRegistry::new())),
            instances: HashMap::new(),
        })
    }

    /// Load a `duckdb:extension` component, run its `load()`, and return the
    /// functions it registered. The instance is retained for dispatch.
    pub fn load(&mut self, extension: &str, path: &Path) -> Result<LoadedComponent> {
        let component = Component::from_file(&self.engine, path)
            .with_context(|| format!("loading component at {}", path.display()))?;
        // Grant outbound network + name lookup so network-using components (dns,
        // http, httpfs, ...) work. Best-effort, not a sandbox: a component that
        // does not use sockets is unaffected. (A future opt-in gate could mirror
        // the host's DUCKLINK_NETWORK_GRANT.)
        let wasi: WasiCtx = WasiCtxBuilder::new()
            .inherit_env()
            .inherit_stdio()
            .inherit_network()
            .allow_ip_name_lookup(true)
            .build();
        let mut instance = load_component(
            &self.engine,
            &component,
            wasi,
            Box::new(NativeServices),
            self.callbacks.clone(),
            extension.to_string(),
        )?;
        let pending: PendingRegistrationsData = instance.drain_pending();
        let scalars = pending
            .scalars
            .into_iter()
            .map(|s| ScalarFunc {
                extension: s.extension,
                name: s.name,
                arguments: s.arguments,
                returns: s.returns,
                callback_handle: s.callback_handle,
            })
            .collect();
        let tables = pending
            .tables
            .into_iter()
            .map(|t| TableFunc {
                extension: t.extension,
                name: t.name,
                arguments: t.arguments,
                columns: t.columns,
                callback_handle: t.callback_handle,
            })
            .collect();
        let aggregates = pending
            .aggregates
            .into_iter()
            .map(|a| AggregateFunc {
                extension: a.extension,
                name: a.name,
                arguments: a.arguments,
                returns: a.returns,
                callback_handle: a.callback_handle,
            })
            .collect();
        self.instances.insert(extension.to_string(), instance);
        Ok(LoadedComponent {
            scalars,
            tables,
            aggregates,
        })
    }

    /// Invoke a component scalar for one row. `callback_handle` is the value
    /// handed to DuckDB at registration; it resolves through the shared callback
    /// registry to the owning component instance and its guest dispatcher.
    pub fn dispatch_scalar(
        &mut self,
        callback_handle: u32,
        row_index: u64,
        args: Vec<reg::DuckValue>,
    ) -> Result<reg::DuckValue> {
        let entry = {
            let registry = self.callbacks.lock().expect("callback registry poisoned");
            registry
                .get(callback_handle)
                .ok_or_else(|| anyhow!("unknown callback handle {callback_handle}"))?
        };
        let instance = self
            .instances
            .get_mut(&entry.extension)
            .ok_or_else(|| anyhow!("extension '{}' is not loaded", entry.extension))?;
        let wit_args: Vec<extension_types::Duckvalue> =
            args.into_iter().map(neutral_to_wit).collect();
        let ctx = extension_runtime::Invokeinfo {
            rowindex: Some(row_index),
            iswindow: false,
        };
        let result = instance
            .dispatch_scalar(entry.dispatcher_handle, &wit_args, ctx)
            .map_err(|e| anyhow!("scalar dispatch failed: {e:?}"))?;
        Ok(wit_to_neutral(result))
    }

    /// Invoke a component scalar over a whole chunk of rows in a single WIT
    /// crossing. `rows[i]` is the argument tuple for row `base_row_index + i`;
    /// the returned vector is the per-row result, one entry per input row. This
    /// collapses the N per-row `dispatch_scalar` boundary crossings of a DuckDB
    /// data chunk into one, which is the dominant cost when N is large.
    pub fn dispatch_scalar_batch(
        &mut self,
        callback_handle: u32,
        base_row_index: u64,
        rows: Vec<Vec<reg::DuckValue>>,
    ) -> Result<Vec<reg::DuckValue>> {
        let entry = {
            let registry = self.callbacks.lock().expect("callback registry poisoned");
            registry
                .get(callback_handle)
                .ok_or_else(|| anyhow!("unknown callback handle {callback_handle}"))?
        };
        let instance = self
            .instances
            .get_mut(&entry.extension)
            .ok_or_else(|| anyhow!("extension '{}' is not loaded", entry.extension))?;
        let wit_rows: Vec<Vec<extension_types::Duckvalue>> = rows
            .into_iter()
            .map(|row| row.into_iter().map(neutral_to_wit).collect())
            .collect();
        let ctx = extension_runtime::Invokeinfo {
            rowindex: Some(base_row_index),
            iswindow: false,
        };
        let result = instance
            .dispatch_scalar_batch(entry.dispatcher_handle, &wit_rows, ctx)
            .map_err(|e| anyhow!("scalar batch dispatch failed: {e:?}"))?;
        Ok(result.into_iter().map(wit_to_neutral).collect())
    }

    /// Invoke a component table function with the given call arguments, returning
    /// all result rows. `callback_handle` resolves through the callback registry
    /// to the owning component instance.
    pub fn dispatch_table(
        &mut self,
        callback_handle: u32,
        args: Vec<reg::DuckValue>,
    ) -> Result<Vec<Vec<reg::DuckValue>>> {
        let entry = {
            let registry = self.callbacks.lock().expect("callback registry poisoned");
            registry
                .get(callback_handle)
                .ok_or_else(|| anyhow!("unknown callback handle {callback_handle}"))?
        };
        let instance = self
            .instances
            .get_mut(&entry.extension)
            .ok_or_else(|| anyhow!("extension '{}' is not loaded", entry.extension))?;
        let wit_args: Vec<extension_types::Duckvalue> =
            args.into_iter().map(neutral_to_wit).collect();
        let rows = instance
            .dispatch_table(entry.dispatcher_handle, &wit_args)
            .map_err(|e| anyhow!("table dispatch failed: {e:?}"))?;
        Ok(rows
            .into_iter()
            .map(|row| row.into_iter().map(wit_to_neutral).collect())
            .collect())
    }

    /// Invoke a component aggregate over all accumulated input `rows` (each row is
    /// the function's argument tuple), returning the single aggregate result. The
    /// component computes the whole aggregate at once. `callback_handle` resolves
    /// through the callback registry to the owning component instance.
    pub fn dispatch_aggregate(
        &mut self,
        callback_handle: u32,
        rows: Vec<Vec<reg::DuckValue>>,
    ) -> Result<reg::DuckValue> {
        let entry = {
            let registry = self.callbacks.lock().expect("callback registry poisoned");
            registry
                .get(callback_handle)
                .ok_or_else(|| anyhow!("unknown callback handle {callback_handle}"))?
        };
        let instance = self
            .instances
            .get_mut(&entry.extension)
            .ok_or_else(|| anyhow!("extension '{}' is not loaded", entry.extension))?;
        let wit_rows: Vec<Vec<extension_types::Duckvalue>> = rows
            .into_iter()
            .map(|row| row.into_iter().map(neutral_to_wit).collect())
            .collect();
        let result = instance
            .dispatch_aggregate(entry.dispatcher_handle, &wit_rows)
            .map_err(|e| anyhow!("aggregate dispatch failed: {e:?}"))?;
        Ok(wit_to_neutral(result))
    }
}

fn neutral_to_wit(v: reg::DuckValue) -> extension_types::Duckvalue {
    match v {
        reg::DuckValue::Null => extension_types::Duckvalue::Null,
        reg::DuckValue::Boolean(b) => extension_types::Duckvalue::Boolean(b),
        reg::DuckValue::Int64(i) => extension_types::Duckvalue::Int64(i),
        reg::DuckValue::Uint64(u) => extension_types::Duckvalue::Uint64(u),
        reg::DuckValue::Float64(f) => extension_types::Duckvalue::Float64(f),
        reg::DuckValue::Text(s) => extension_types::Duckvalue::Text(s),
        reg::DuckValue::Blob(b) => extension_types::Duckvalue::Blob(b),
    }
}

fn wit_to_neutral(v: extension_types::Duckvalue) -> reg::DuckValue {
    match v {
        extension_types::Duckvalue::Null => reg::DuckValue::Null,
        extension_types::Duckvalue::Boolean(b) => reg::DuckValue::Boolean(b),
        extension_types::Duckvalue::Int64(i) => reg::DuckValue::Int64(i),
        extension_types::Duckvalue::Uint64(u) => reg::DuckValue::Uint64(u),
        extension_types::Duckvalue::Float64(f) => reg::DuckValue::Float64(f),
        extension_types::Duckvalue::Text(s) => reg::DuckValue::Text(s),
        extension_types::Duckvalue::Blob(b) => reg::DuckValue::Blob(b),
    }
}
