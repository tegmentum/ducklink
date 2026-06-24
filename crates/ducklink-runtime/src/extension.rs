//! The reusable extension store-state + loaded-component instance.
//!
//! `ExtensionStoreState` implements the `duckdb:extension` host capability
//! traits: it captures what a component's `load()` registers (into the neutral
//! [`crate::reg`] model) and services the component's config/logging requests
//! through an [`ExtensionServices`] sink. The sink is the one direction-specific
//! seam — the `ducklink` host routes it to DuckDB-compiled-to-wasm; the native
//! `ducklink` extension will route it to native DuckDB.
//!
//! `ExtensionInstance` is a loaded component: its `Store<ExtensionStoreState>`
//! plus generated bindings, with `dispatch_*` re-entering the guest's
//! `callback-dispatch` export for each DuckDB-side invocation.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use wasmtime::component::{Component, Linker, Resource, ResourceTable};
use wasmtime::{AsContextMut, Engine, Store};
use wasmtime_wasi::{WasiCtx, WasiCtxView, WasiView};

use crate::duckdb_extension_bindings::duckdb::extension::{
    catalog as extension_catalog, config as extension_config, files as extension_files,
    logging as extension_logging, runtime as extension_runtime, storage as extension_storage,
    types as extension_types,
};
use crate::duckdb_extension_bindings::{DuckdbExtension, DuckdbExtensionPre};
use crate::reg;
use crate::{CallbackKind, CallbackRegistry};

type BindgenVec<T> = wasmtime::component::__internal::Vec<T>;

// ---------------------------------------------------------------------------
// Service sink (the one direction-specific seam)
// ---------------------------------------------------------------------------

/// A configuration error surfaced to a component. Neutral mirror of
/// `duckdb:extension/types.config-error`.
#[derive(Debug, Clone)]
pub enum ConfigError {
    InvalidKey(String),
    TypeMismatch(String),
    Unavailable(String),
    InternalConfig(String),
}

/// A log severity. Neutral mirror of `duckdb:extension/logging.log-level`.
#[derive(Debug, Clone, Copy)]
pub enum LogLevel {
    Trace,
    Debug,
    Info,
    Warn,
    Error,
}

/// A structured log field (key/value). Neutral mirror of
/// `duckdb:extension/logging.log-field`.
#[derive(Debug, Clone)]
pub struct LogField {
    pub key: String,
    pub value: String,
}

/// Services a loaded component requests from the running database: reading
/// configuration and emitting logs. Implemented per direction (the host routes
/// to DuckDB-compiled-to-wasm; the native extension to native DuckDB).
///
/// `Send` so `ExtensionStoreState` can move across the loader thread.
pub trait ExtensionServices: Send {
    fn provider_version(&mut self) -> Result<String, ConfigError>;
    fn list_keys(&mut self, prefix: Option<&str>) -> Result<Vec<String>, ConfigError>;
    fn get_string(&mut self, path: &str) -> Result<Option<String>, ConfigError>;
    fn get_bool(&mut self, path: &str) -> Result<Option<bool>, ConfigError>;
    fn get_i64(&mut self, path: &str) -> Result<Option<i64>, ConfigError>;
    fn get_u64(&mut self, path: &str) -> Result<Option<u64>, ConfigError>;
    fn get_f64(&mut self, path: &str) -> Result<Option<f64>, ConfigError>;
    fn get_bytes(&mut self, path: &str) -> Result<Option<Vec<u8>>, ConfigError>;
    fn get_string_list(&mut self, path: &str) -> Result<Option<Vec<String>>, ConfigError>;
    fn log(&mut self, level: LogLevel, message: &str, target: Option<&str>);
    fn log_fields(&mut self, level: LogLevel, message: &str, fields: &[LogField]);
}

fn neutral_configerror_to_ext(err: ConfigError) -> extension_types::Configerror {
    match err {
        ConfigError::InvalidKey(m) => extension_types::Configerror::Invalidkey(m),
        ConfigError::TypeMismatch(m) => extension_types::Configerror::Typemismatch(m),
        ConfigError::Unavailable(m) => extension_types::Configerror::Unavailable(m),
        ConfigError::InternalConfig(m) => extension_types::Configerror::Internalconfig(m),
    }
}

fn ext_loglevel_to_neutral(level: extension_logging::Loglevel) -> LogLevel {
    match level {
        extension_logging::Loglevel::Trace => LogLevel::Trace,
        extension_logging::Loglevel::Debug => LogLevel::Debug,
        extension_logging::Loglevel::Info => LogLevel::Info,
        extension_logging::Loglevel::Warn => LogLevel::Warn,
        extension_logging::Loglevel::Error => LogLevel::Error,
    }
}

// ---------------------------------------------------------------------------
// Pending-registration buffers
// ---------------------------------------------------------------------------

type PendingScalar = reg::ScalarReg;
type PendingTable = reg::TableReg;
type PendingAggregate = reg::AggregateReg;
type PendingMacro = reg::MacroReg;
type PendingReplacementScan = reg::ReplacementScanReg;
type PendingLogicalType = reg::LogicalTypeReg;
type PendingCast = reg::CastReg;
type PendingStorage = reg::StorageReg;

#[derive(Default)]
struct PendingScalarRegistry {
    entries: Vec<PendingScalar>,
}

#[derive(Default)]
struct PendingTableRegistry {
    entries: Vec<PendingTable>,
}

#[derive(Default)]
struct PendingAggregateRegistry {
    entries: Vec<PendingAggregate>,
}

/// The full set of registrations captured from one or more components, ready
/// for a direction-specific sink to forward into the database.
#[derive(Default)]
pub struct PendingRegistrationsData {
    pub scalars: Vec<PendingScalar>,
    pub tables: Vec<PendingTable>,
    pub aggregates: Vec<PendingAggregate>,
    pub macros: Vec<PendingMacro>,
    pub replacement_scans: Vec<PendingReplacementScan>,
    pub logical_types: Vec<PendingLogicalType>,
    pub casts: Vec<PendingCast>,
    pub storages: Vec<PendingStorage>,
}

impl PendingRegistrationsData {
    pub fn append(&mut self, mut other: PendingRegistrationsData) {
        self.scalars.append(&mut other.scalars);
        self.tables.append(&mut other.tables);
        self.aggregates.append(&mut other.aggregates);
        self.macros.append(&mut other.macros);
        self.replacement_scans.append(&mut other.replacement_scans);
        self.logical_types.append(&mut other.logical_types);
        self.casts.append(&mut other.casts);
        self.storages.append(&mut other.storages);
    }
}

pub fn summarize_registration_names<T, F>(entries: &[T], mut project: F) -> String
where
    F: FnMut(&T) -> &str,
{
    if entries.is_empty() {
        return "none".to_string();
    }
    const PREVIEW: usize = 3;
    let mut listed: Vec<String> = entries
        .iter()
        .take(PREVIEW)
        .map(|entry| project(entry).to_string())
        .collect();
    if entries.len() > PREVIEW {
        listed.push(format!("+{} more", entries.len() - PREVIEW));
    }
    listed.join(", ")
}

// ---------------------------------------------------------------------------
// ExtensionStoreState
// ---------------------------------------------------------------------------

/// Per-component wasmtime store data: wasi context + capability capture buffers
/// + the config/logging sink + the shared callback registry.
pub struct ExtensionStoreState {
    table: ResourceTable,
    wasi: WasiCtx,
    services: Box<dyn ExtensionServices>,
    next_resource_id: u32,
    scalar_registries: HashMap<u32, PendingScalarRegistry>,
    table_registries: HashMap<u32, PendingTableRegistry>,
    aggregate_registries: HashMap<u32, PendingAggregateRegistry>,
    // Registrations are retained here once their registry resource is dropped by
    // the guest (which happens as soon as `load()` returns), so they survive
    // until `drain_pending` forwards them to the sink.
    pending_scalars: Vec<PendingScalar>,
    pending_tables: Vec<PendingTable>,
    pending_aggregates: Vec<PendingAggregate>,
    pending_macros: Vec<PendingMacro>,
    pending_replacement_scans: Vec<PendingReplacementScan>,
    pending_logical_types: Vec<PendingLogicalType>,
    pending_casts: Vec<PendingCast>,
    pending_storages: Vec<PendingStorage>,
    /// Maps the handle returned from `table-registry.register` to the table
    /// function name, so `files.register-replacement-scan` can resolve it.
    table_handle_names: HashMap<u32, String>,
    callback_registry: Arc<Mutex<CallbackRegistry>>,
    extension_name: String,
}

impl ExtensionStoreState {
    pub fn new(
        wasi: WasiCtx,
        services: Box<dyn ExtensionServices>,
        callback_registry: Arc<Mutex<CallbackRegistry>>,
        extension_name: String,
    ) -> Self {
        Self {
            table: ResourceTable::new(),
            wasi,
            services,
            next_resource_id: 1,
            scalar_registries: HashMap::new(),
            table_registries: HashMap::new(),
            aggregate_registries: HashMap::new(),
            pending_scalars: Vec::new(),
            pending_tables: Vec::new(),
            pending_aggregates: Vec::new(),
            pending_macros: Vec::new(),
            pending_replacement_scans: Vec::new(),
            pending_logical_types: Vec::new(),
            pending_casts: Vec::new(),
            pending_storages: Vec::new(),
            table_handle_names: HashMap::new(),
            callback_registry,
            extension_name,
        }
    }

    fn alloc_resource_id(&mut self) -> u32 {
        let id = self.next_resource_id;
        self.next_resource_id = self.next_resource_id.wrapping_add(1).max(1);
        id
    }

    fn allocate_callback_handle(&self, dispatcher_handle: u32, kind: CallbackKind) -> u32 {
        let mut registry = self
            .callback_registry
            .lock()
            .expect("callback registry mutex poisoned");
        registry.allocate(&self.extension_name, kind, dispatcher_handle)
    }

    fn release_callback_handle(&self, handle: u32) {
        let mut registry = self
            .callback_registry
            .lock()
            .expect("callback registry mutex poisoned");
        registry.remove(handle);
    }

    fn drain_pending(&mut self) -> PendingRegistrationsData {
        // Combine registrations retained from dropped registries with any that
        // belong to registries still held alive by the guest.
        let mut scalars = std::mem::take(&mut self.pending_scalars);
        scalars.extend(
            self.scalar_registries
                .drain()
                .flat_map(|(_, registry)| registry.entries),
        );
        let mut tables = std::mem::take(&mut self.pending_tables);
        tables.extend(
            self.table_registries
                .drain()
                .flat_map(|(_, registry)| registry.entries),
        );
        let mut aggregates = std::mem::take(&mut self.pending_aggregates);
        aggregates.extend(
            self.aggregate_registries
                .drain()
                .flat_map(|(_, registry)| registry.entries),
        );
        let macros = std::mem::take(&mut self.pending_macros);
        let replacement_scans = std::mem::take(&mut self.pending_replacement_scans);
        let logical_types = std::mem::take(&mut self.pending_logical_types);
        let casts = std::mem::take(&mut self.pending_casts);
        let storages = std::mem::take(&mut self.pending_storages);
        let pending = PendingRegistrationsData {
            scalars,
            tables,
            aggregates,
            macros,
            replacement_scans,
            logical_types,
            casts,
            storages,
        };
        let scalar_names =
            summarize_registration_names(&pending.scalars, |entry| entry.name.as_str());
        let table_names =
            summarize_registration_names(&pending.tables, |entry| entry.name.as_str());
        let aggregate_names =
            summarize_registration_names(&pending.aggregates, |entry| entry.name.as_str());
        let macro_names =
            summarize_registration_names(&pending.macros, |entry| entry.name.as_str());
        eprintln!(
            "[extension-runtime:{}] draining pending registrations: scalars={} ({scalar_names}), tables={} ({table_names}), aggregates={} ({aggregate_names}), macros={} ({macro_names})",
            self.extension_name,
            pending.scalars.len(),
            pending.tables.len(),
            pending.aggregates.len(),
            pending.macros.len()
        );
        pending
    }
}

impl WasiView for ExtensionStoreState {
    fn ctx(&mut self) -> WasiCtxView<'_> {
        WasiCtxView {
            ctx: &mut self.wasi,
            table: &mut self.table,
        }
    }
}

impl wasmtime::component::HasData for ExtensionStoreState {
    type Data<'a> = &'a mut ExtensionStoreState;
}

fn unsupported_runtime_error() -> extension_types::Duckerror {
    extension_types::Duckerror::Unsupported(
        "component runtime not available in CLI host".to_string(),
    )
}

impl extension_types::Host for ExtensionStoreState {}

impl extension_runtime::Host for ExtensionStoreState {
    fn get_capability(
        &mut self,
        kind: extension_runtime::Capabilitykind,
    ) -> Option<extension_runtime::Capability> {
        match kind {
            extension_runtime::Capabilitykind::Scalar => {
                let id = self.alloc_resource_id();
                self.scalar_registries
                    .insert(id, PendingScalarRegistry::default());
                Some(extension_runtime::Capability::Scalar(
                    wasmtime::component::Resource::new_own(id),
                ))
            }
            extension_runtime::Capabilitykind::Table => {
                let id = self.alloc_resource_id();
                self.table_registries
                    .insert(id, PendingTableRegistry::default());
                Some(extension_runtime::Capability::Table(
                    wasmtime::component::Resource::new_own(id),
                ))
            }
            extension_runtime::Capabilitykind::Aggregate => {
                let id = self.alloc_resource_id();
                self.aggregate_registries
                    .insert(id, PendingAggregateRegistry::default());
                Some(extension_runtime::Capability::Aggregate(
                    wasmtime::component::Resource::new_own(id),
                ))
            }
            _ => None,
        }
    }

    fn list_capabilities(&mut self) -> BindgenVec<extension_runtime::Capabilitykind> {
        vec![
            extension_runtime::Capabilitykind::Scalar,
            extension_runtime::Capabilitykind::Table,
            extension_runtime::Capabilitykind::Aggregate,
        ]
        .into()
    }
}

impl extension_runtime::HostScalarCallback for ExtensionStoreState {
    fn new(&mut self, handle: u32) -> Resource<extension_runtime::ScalarCallback> {
        let id = self.allocate_callback_handle(handle, CallbackKind::Scalar);
        wasmtime::component::Resource::new_own(id)
    }

    fn call(
        &mut self,
        _self_: Resource<extension_runtime::ScalarCallback>,
        _args: BindgenVec<extension_types::Duckvalue>,
        _ctx: extension_runtime::Invokeinfo,
    ) -> Result<extension_types::Duckvalue, extension_types::Duckerror> {
        Err(unsupported_runtime_error())
    }

    fn drop(&mut self, rep: Resource<extension_runtime::ScalarCallback>) -> wasmtime::Result<()> {
        self.release_callback_handle(rep.rep());
        Ok(())
    }
}

impl extension_runtime::HostTableCallback for ExtensionStoreState {
    fn new(&mut self, handle: u32) -> Resource<extension_runtime::TableCallback> {
        let id = self.allocate_callback_handle(handle, CallbackKind::Table);
        wasmtime::component::Resource::new_own(id)
    }

    fn call(
        &mut self,
        _self_: Resource<extension_runtime::TableCallback>,
        _args: BindgenVec<extension_types::Duckvalue>,
    ) -> Result<extension_runtime::Resultset, extension_types::Duckerror> {
        Err(unsupported_runtime_error())
    }

    fn drop(&mut self, rep: Resource<extension_runtime::TableCallback>) -> wasmtime::Result<()> {
        self.release_callback_handle(rep.rep());
        Ok(())
    }
}

impl extension_runtime::HostAggregateCallback for ExtensionStoreState {
    fn new(&mut self, handle: u32) -> Resource<extension_runtime::AggregateCallback> {
        let id = self.allocate_callback_handle(handle, CallbackKind::Aggregate);
        wasmtime::component::Resource::new_own(id)
    }

    fn call(
        &mut self,
        _self_: Resource<extension_runtime::AggregateCallback>,
        _rows: extension_runtime::Rowbatch,
    ) -> Result<extension_types::Duckvalue, extension_types::Duckerror> {
        Err(unsupported_runtime_error())
    }

    fn drop(&mut self, rep: Resource<extension_runtime::AggregateCallback>) -> wasmtime::Result<()> {
        self.release_callback_handle(rep.rep());
        Ok(())
    }
}

impl extension_runtime::HostPragmaCallback for ExtensionStoreState {
    fn new(&mut self, handle: u32) -> Resource<extension_runtime::PragmaCallback> {
        let id = self.allocate_callback_handle(handle, CallbackKind::Pragma);
        wasmtime::component::Resource::new_own(id)
    }

    fn call(
        &mut self,
        _self_: Resource<extension_runtime::PragmaCallback>,
        _args: BindgenVec<extension_types::Duckvalue>,
    ) -> Result<Option<extension_types::Duckvalue>, extension_types::Duckerror> {
        Err(unsupported_runtime_error())
    }

    fn drop(&mut self, rep: Resource<extension_runtime::PragmaCallback>) -> wasmtime::Result<()> {
        self.release_callback_handle(rep.rep());
        Ok(())
    }
}

impl extension_runtime::HostCastCallback for ExtensionStoreState {
    fn new(&mut self, handle: u32) -> Resource<extension_runtime::CastCallback> {
        let id = self.allocate_callback_handle(handle, CallbackKind::Cast);
        wasmtime::component::Resource::new_own(id)
    }

    fn call(
        &mut self,
        _self_: Resource<extension_runtime::CastCallback>,
        _value: extension_types::Duckvalue,
    ) -> Result<extension_types::Duckvalue, extension_types::Duckerror> {
        Err(unsupported_runtime_error())
    }

    fn drop(&mut self, rep: Resource<extension_runtime::CastCallback>) -> wasmtime::Result<()> {
        self.release_callback_handle(rep.rep());
        Ok(())
    }
}

impl extension_runtime::HostScalarRegistry for ExtensionStoreState {
    fn register(
        &mut self,
        self_: Resource<extension_runtime::ScalarRegistry>,
        name: String,
        arguments: BindgenVec<extension_runtime::Funcarg>,
        returns: extension_runtime::Logicaltype,
        callback: Resource<extension_runtime::ScalarCallback>,
        options: Option<extension_runtime::Funcopts>,
    ) -> Result<u32, extension_types::Duckerror> {
        {
            let registry = self
                .callback_registry
                .lock()
                .expect("callback registry mutex poisoned");
            match registry.get(callback.rep()) {
                Some(entry) if entry.kind == CallbackKind::Scalar => {}
                Some(_) => {
                    return Err(extension_types::Duckerror::Invalidargument(
                        "callback handle is not scalar".to_string(),
                    ))
                }
                None => {
                    return Err(extension_types::Duckerror::Internal(
                        "unknown scalar callback handle".to_string(),
                    ))
                }
            }
        }

        let registry_id = self_.rep();
        let registry = self.scalar_registries.get_mut(&registry_id).ok_or_else(|| {
            extension_types::Duckerror::Internal("unknown scalar registry handle".to_string())
        })?;

        let callback_handle = callback.rep();
        std::mem::forget(callback);

        let converted_arguments = convert_extension_funcargs(arguments.into());
        let converted_returns = convert_extension_logicaltype(returns);
        let converted_options = options.map(convert_extension_funcopts);
        log_scalar_registration(
            &self.extension_name,
            &name,
            registry_id,
            callback_handle,
            &converted_arguments,
            &converted_returns,
            converted_options.as_ref(),
        );

        registry.entries.push(PendingScalar {
            extension: self.extension_name.clone(),
            name,
            arguments: converted_arguments,
            returns: converted_returns,
            callback_handle,
            options: converted_options,
        });

        Ok(self.alloc_resource_id())
    }

    fn drop(&mut self, rep: Resource<extension_runtime::ScalarRegistry>) -> wasmtime::Result<()> {
        if let Some(registry) = self.scalar_registries.remove(&rep.rep()) {
            self.pending_scalars.extend(registry.entries);
        }
        Ok(())
    }
}

impl extension_runtime::HostTableRegistry for ExtensionStoreState {
    fn register(
        &mut self,
        self_: Resource<extension_runtime::TableRegistry>,
        name: String,
        arguments: BindgenVec<extension_runtime::Funcarg>,
        columns: BindgenVec<extension_runtime::Columndef>,
        callback: Resource<extension_runtime::TableCallback>,
        options: Option<extension_runtime::Extopts>,
    ) -> Result<u32, extension_types::Duckerror> {
        {
            let registry = self
                .callback_registry
                .lock()
                .expect("callback registry mutex poisoned");
            match registry.get(callback.rep()) {
                Some(entry) if entry.kind == CallbackKind::Table => {}
                Some(_) => {
                    return Err(extension_types::Duckerror::Invalidargument(
                        "callback handle is not a table callback".to_string(),
                    ))
                }
                None => {
                    return Err(extension_types::Duckerror::Internal(
                        "unknown table callback handle".to_string(),
                    ))
                }
            }
        }

        let registry_id = self_.rep();
        let registry = self.table_registries.get_mut(&registry_id).ok_or_else(|| {
            extension_types::Duckerror::Internal("unknown table registry handle".to_string())
        })?;

        let callback_handle = callback.rep();
        std::mem::forget(callback);

        let converted_arguments = convert_extension_funcargs(arguments.into());
        let converted_columns = convert_extension_columndefs(columns.into());
        let converted_options = options.map(convert_extension_extopts);
        log_table_registration(
            &self.extension_name,
            &name,
            registry_id,
            callback_handle,
            &converted_arguments,
            &converted_columns,
            converted_options.as_ref(),
        );

        let table_name = name.clone();
        registry.entries.push(PendingTable {
            extension: self.extension_name.clone(),
            name,
            arguments: converted_arguments,
            columns: converted_columns,
            callback_handle,
            options: converted_options,
        });

        // The returned handle is what the extension later passes to
        // `files.register-replacement-scan`; remember which table function it
        // names so we can resolve it.
        let handle = self.alloc_resource_id();
        self.table_handle_names.insert(handle, table_name);
        Ok(handle)
    }

    fn drop(&mut self, rep: Resource<extension_runtime::TableRegistry>) -> wasmtime::Result<()> {
        if let Some(registry) = self.table_registries.remove(&rep.rep()) {
            self.pending_tables.extend(registry.entries);
        }
        Ok(())
    }
}

impl extension_runtime::HostAggregateRegistry for ExtensionStoreState {
    fn register(
        &mut self,
        self_: Resource<extension_runtime::AggregateRegistry>,
        name: String,
        arguments: BindgenVec<extension_runtime::Funcarg>,
        returns: extension_runtime::Logicaltype,
        callback: Resource<extension_runtime::AggregateCallback>,
        options: Option<extension_runtime::Funcopts>,
    ) -> Result<u32, extension_types::Duckerror> {
        {
            let registry = self
                .callback_registry
                .lock()
                .expect("callback registry mutex poisoned");
            match registry.get(callback.rep()) {
                Some(entry) if entry.kind == CallbackKind::Aggregate => {}
                Some(_) => {
                    return Err(extension_types::Duckerror::Invalidargument(
                        "callback handle is not aggregate".to_string(),
                    ))
                }
                None => {
                    return Err(extension_types::Duckerror::Internal(
                        "unknown aggregate callback handle".to_string(),
                    ))
                }
            }
        }

        let registry_id = self_.rep();
        let registry = self
            .aggregate_registries
            .get_mut(&registry_id)
            .ok_or_else(|| {
                extension_types::Duckerror::Internal(
                    "unknown aggregate registry handle".to_string(),
                )
            })?;

        let callback_handle = callback.rep();
        std::mem::forget(callback);

        let converted_arguments = convert_extension_funcargs(arguments.into());
        let converted_returns = convert_extension_logicaltype(returns);
        let converted_options = options.map(convert_extension_funcopts);
        log_aggregate_registration(
            &self.extension_name,
            &name,
            registry_id,
            callback_handle,
            &converted_arguments,
            &converted_returns,
            converted_options.as_ref(),
        );

        registry.entries.push(PendingAggregate {
            extension: self.extension_name.clone(),
            name,
            arguments: converted_arguments,
            returns: converted_returns,
            callback_handle,
            options: converted_options,
        });

        Ok(self.alloc_resource_id())
    }

    fn drop(&mut self, rep: Resource<extension_runtime::AggregateRegistry>) -> wasmtime::Result<()> {
        if let Some(registry) = self.aggregate_registries.remove(&rep.rep()) {
            self.pending_aggregates.extend(registry.entries);
        }
        Ok(())
    }
}

impl extension_runtime::HostPragmaRegistry for ExtensionStoreState {
    fn register_call(
        &mut self,
        _self_: Resource<extension_runtime::PragmaRegistry>,
        _name: String,
        _arguments: BindgenVec<extension_runtime::Funcarg>,
        _returns: extension_runtime::Logicaltype,
        _callback: Resource<extension_runtime::PragmaCallback>,
        _options: Option<extension_runtime::Extopts>,
    ) -> Result<u32, extension_types::Duckerror> {
        Err(unsupported_runtime_error())
    }

    fn drop(&mut self, _rep: Resource<extension_runtime::PragmaRegistry>) -> wasmtime::Result<()> {
        Ok(())
    }
}

impl extension_runtime::HostMacroRegistry for ExtensionStoreState {
    fn register_scalar(
        &mut self,
        _self_: Resource<extension_runtime::MacroRegistry>,
        _name: String,
        _parameters: BindgenVec<String>,
        _body_sql: String,
        _options: Option<extension_runtime::Extopts>,
    ) -> Result<bool, extension_types::Duckerror> {
        Err(unsupported_runtime_error())
    }

    fn drop(&mut self, _rep: Resource<extension_runtime::MacroRegistry>) -> wasmtime::Result<()> {
        Ok(())
    }
}

impl extension_config::Host for ExtensionStoreState {
    fn provider_version(&mut self) -> String {
        self.services.provider_version().unwrap_or_else(|err| {
            eprintln!("extension config provider-version failed: {err:?}");
            "duckdb-extension-host".into()
        })
    }

    fn list_keys(&mut self, prefix: Option<String>) -> BindgenVec<String> {
        self.services
            .list_keys(prefix.as_deref())
            .unwrap_or_else(|err| {
                eprintln!("extension config list-keys failed: {err:?}");
                Vec::new()
            })
            .into()
    }

    fn get_string(&mut self, path: String) -> Result<Option<String>, extension_types::Configerror> {
        self.services
            .get_string(&path)
            .map_err(neutral_configerror_to_ext)
    }

    fn get_bool(&mut self, path: String) -> Result<Option<bool>, extension_types::Configerror> {
        self.services
            .get_bool(&path)
            .map_err(neutral_configerror_to_ext)
    }

    fn get_i64(&mut self, path: String) -> Result<Option<i64>, extension_types::Configerror> {
        self.services
            .get_i64(&path)
            .map_err(neutral_configerror_to_ext)
    }

    fn get_u64(&mut self, path: String) -> Result<Option<u64>, extension_types::Configerror> {
        self.services
            .get_u64(&path)
            .map_err(neutral_configerror_to_ext)
    }

    fn get_f64(&mut self, path: String) -> Result<Option<f64>, extension_types::Configerror> {
        self.services
            .get_f64(&path)
            .map_err(neutral_configerror_to_ext)
    }

    fn get_bytes(
        &mut self,
        path: String,
    ) -> Result<Option<BindgenVec<u8>>, extension_types::Configerror> {
        let value = self
            .services
            .get_bytes(&path)
            .map_err(neutral_configerror_to_ext)?;
        Ok(value.map(|bytes| bytes.into()))
    }

    fn get_string_list(
        &mut self,
        path: String,
    ) -> Result<Option<BindgenVec<String>>, extension_types::Configerror> {
        let value = self
            .services
            .get_string_list(&path)
            .map_err(neutral_configerror_to_ext)?;
        Ok(value.map(|items| items.into()))
    }
}

impl extension_logging::Host for ExtensionStoreState {
    fn log(&mut self, level: extension_logging::Loglevel, message: String, target: Option<String>) {
        self.services
            .log(ext_loglevel_to_neutral(level), &message, target.as_deref());
    }

    fn log_fields(
        &mut self,
        level: extension_logging::Loglevel,
        message: String,
        fields: BindgenVec<extension_logging::Logfield>,
    ) {
        let converted: Vec<LogField> = fields
            .into_iter()
            .map(|field| LogField {
                key: field.key.into(),
                value: field.value.into(),
            })
            .collect();
        self.services
            .log_fields(ext_loglevel_to_neutral(level), &message, &converted);
    }
}

// The `catalog` and `files` interfaces are part of the extension world so that
// extensions can register logical types, casts, macros, replacement scans, and
// copy handlers. The host satisfies the imports here so such extensions
// instantiate and load; the requests are captured into the neutral pending
// buffers. Forwarding them into DuckDB is the direction-specific sink's job.
impl extension_catalog::Host for ExtensionStoreState {
    fn register_logical_type(
        &mut self,
        ty: extension_catalog::LogicalType,
    ) -> Result<u32, String> {
        let handle = self.alloc_resource_id();
        eprintln!(
            "[extension-manager] catalog register-logical-type '{}' (physical={}) for '{}' -> handle {handle}",
            ty.name, ty.physical, self.extension_name
        );
        self.pending_logical_types.push(PendingLogicalType {
            extension: self.extension_name.clone(),
            name: ty.name,
            physical: ty.physical,
        });
        Ok(handle)
    }

    fn register_cast(
        &mut self,
        spec: extension_catalog::CastSpec,
        callback: Resource<extension_catalog::CastCallback>,
    ) -> Result<(), String> {
        let callback_handle = callback.rep();
        std::mem::forget(callback);
        eprintln!(
            "[extension-manager] catalog register-cast {}->{} ({:?}, callback={callback_handle}) for '{}'",
            spec.from, spec.to, spec.kind, self.extension_name
        );
        self.pending_casts.push(PendingCast {
            extension: self.extension_name.clone(),
            source: spec.from,
            target: spec.to,
            callback_handle,
        });
        Ok(())
    }

    fn register_macro(&mut self, def: extension_catalog::MacroDef) -> Result<(), String> {
        eprintln!(
            "[extension-manager] catalog register-macro '{}.{}' ({} params) for '{}'",
            def.schema,
            def.name,
            def.parameters.len(),
            self.extension_name
        );
        self.pending_macros.push(PendingMacro {
            extension: self.extension_name.clone(),
            schema: def.schema,
            name: def.name,
            parameters: def.parameters.into_iter().collect(),
            definition_sql: def.definition_sql,
        });
        Ok(())
    }
}

impl extension_files::Host for ExtensionStoreState {
    fn register_replacement_scan(
        &mut self,
        scan: extension_files::ReplacementScan,
    ) -> Result<u32, String> {
        let function_name = self
            .table_handle_names
            .get(&scan.table_function)
            .cloned()
            .ok_or_else(|| {
                format!(
                    "replacement scan references unknown table-function handle {}",
                    scan.table_function
                )
            })?;
        let id = self.alloc_resource_id();
        let extensions: Vec<String> = scan.extensions.into_iter().collect();
        eprintln!(
            "[extension-manager] files register-replacement-scan exts={:?} ({:?}) -> '{}' for '{}' (id {id})",
            extensions, scan.mode, function_name, self.extension_name
        );
        self.pending_replacement_scans.push(PendingReplacementScan {
            extension: self.extension_name.clone(),
            extensions,
            function_name,
        });
        Ok(id)
    }

    fn register_copy_handler(
        &mut self,
        handler: extension_files::CopyHandler,
    ) -> Result<u32, String> {
        // DuckDB's C API exposes no copy-function registration, so this cannot
        // be honoured. Fail loudly rather than silently pretending it worked.
        eprintln!(
            "[extension-manager] files register-copy-handler ext='{}' for '{}' rejected: unsupported",
            handler.extension, self.extension_name
        );
        Err(
            "copy handlers are not supported: DuckDB's C API has no copy-function registration"
                .to_string(),
        )
    }
}

// The `storage` interface lets a component register an ATTACH-able catalog
// backend (a DB scanner) in `load()`. The host satisfies the import so
// storage-capable components instantiate and load; the registration is captured
// into the neutral pending buffer. Driving the component's `storage-dispatch`
// export (attach/scan) is the direction-specific sink's job.
impl extension_storage::Host for ExtensionStoreState {
    fn register_storage(
        &mut self,
        type_name: String,
        callback_handle: u32,
        options: Option<extension_storage::Extopts>,
    ) -> Result<u32, extension_types::Duckerror> {
        let converted_options = options.map(convert_storage_extopts);
        eprintln!(
            "[extension-runtime:{}] registered storage backend '{type_name}' (callback={callback_handle})",
            self.extension_name
        );
        self.pending_storages.push(PendingStorage {
            extension: self.extension_name.clone(),
            type_name,
            callback_handle,
            options: converted_options,
        });
        Ok(self.alloc_resource_id())
    }
}

// ---------------------------------------------------------------------------
// Capture conversions (extension WIT -> neutral reg::*) + logging helpers
// ---------------------------------------------------------------------------

fn convert_extension_funcargs(args: Vec<extension_runtime::Funcarg>) -> Vec<reg::FuncArg> {
    args.into_iter()
        .map(|arg| reg::FuncArg {
            name: arg.name,
            logical: convert_extension_logicaltype(arg.logical),
        })
        .collect()
}

fn convert_extension_logicaltype(ty: extension_runtime::Logicaltype) -> reg::LogicalType {
    match ty {
        extension_runtime::Logicaltype::Boolean => reg::LogicalType::Boolean,
        extension_runtime::Logicaltype::Int64 => reg::LogicalType::Int64,
        extension_runtime::Logicaltype::Uint64 => reg::LogicalType::Uint64,
        extension_runtime::Logicaltype::Float64 => reg::LogicalType::Float64,
        extension_runtime::Logicaltype::Text => reg::LogicalType::Text,
        extension_runtime::Logicaltype::Blob => reg::LogicalType::Blob,
    }
}

fn convert_extension_funcopts(opts: extension_runtime::Funcopts) -> reg::FuncOpts {
    reg::FuncOpts {
        description: opts.description,
        tags: opts.tags.into_iter().collect(),
        attributes: convert_extension_funcflags(opts.attributes),
    }
}

fn convert_extension_columndefs(columns: Vec<extension_runtime::Columndef>) -> Vec<reg::ColumnDef> {
    columns
        .into_iter()
        .map(|col| reg::ColumnDef {
            name: col.name,
            logical: convert_extension_logicaltype(col.logical),
        })
        .collect()
}

fn convert_extension_extopts(opts: extension_runtime::Extopts) -> reg::ExtOpts {
    reg::ExtOpts {
        description: opts.description,
        tags: opts.tags.into_iter().collect(),
    }
}

fn convert_storage_extopts(opts: extension_storage::Extopts) -> reg::ExtOpts {
    reg::ExtOpts {
        description: opts.description,
        tags: opts.tags.into_iter().collect(),
    }
}

fn convert_extension_funcflags(flags: extension_types::Funcflags) -> reg::FuncFlags {
    reg::FuncFlags {
        deterministic: flags.contains(extension_types::Funcflags::DETERMINISTIC),
        commutative: flags.contains(extension_types::Funcflags::COMMUTATIVE),
        stateless: flags.contains(extension_types::Funcflags::STATELESS),
        side_effecting: flags.contains(extension_types::Funcflags::SIDEEFFECTING),
        deprecated: flags.contains(extension_types::Funcflags::DEPRECATED),
    }
}

fn log_scalar_registration(
    extension: &str,
    name: &str,
    registry_id: u32,
    callback_handle: u32,
    args: &[reg::FuncArg],
    returns: &reg::LogicalType,
    options: Option<&reg::FuncOpts>,
) {
    let arg_summary = summarize_runtime_funcargs(args);
    let return_ty = describe_runtime_logicaltype(returns);
    let option_summary = summarize_funcopts(options);
    eprintln!(
        "[extension-runtime:{extension}] queued scalar '{name}' (registry={registry_id}, callback={callback_handle}) args={arg_summary} returns={return_ty} opts={option_summary}"
    );
}

fn log_table_registration(
    extension: &str,
    name: &str,
    registry_id: u32,
    callback_handle: u32,
    args: &[reg::FuncArg],
    columns: &[reg::ColumnDef],
    options: Option<&reg::ExtOpts>,
) {
    let arg_summary = summarize_runtime_funcargs(args);
    let column_summary = summarize_runtime_columns(columns);
    let option_summary = summarize_extopts(options);
    eprintln!(
        "[extension-runtime:{extension}] queued table '{name}' (registry={registry_id}, callback={callback_handle}) args={arg_summary} columns={column_summary} opts={option_summary}"
    );
}

fn log_aggregate_registration(
    extension: &str,
    name: &str,
    registry_id: u32,
    callback_handle: u32,
    args: &[reg::FuncArg],
    returns: &reg::LogicalType,
    options: Option<&reg::FuncOpts>,
) {
    let arg_summary = summarize_runtime_funcargs(args);
    let return_ty = describe_runtime_logicaltype(returns);
    let option_summary = summarize_funcopts(options);
    eprintln!(
        "[extension-runtime:{extension}] queued aggregate '{name}' (registry={registry_id}, callback={callback_handle}) args={arg_summary} returns={return_ty} opts={option_summary}"
    );
}

pub fn summarize_runtime_funcargs(args: &[reg::FuncArg]) -> String {
    if args.is_empty() {
        return "[]".to_string();
    }
    let parts: Vec<String> = args
        .iter()
        .map(|arg| {
            let name = arg.name.as_ref().map(|s| s.as_str()).unwrap_or("-");
            format!("{name}:{}", describe_runtime_logicaltype(&arg.logical))
        })
        .collect();
    format!("[{}]", parts.join(", "))
}

pub fn summarize_runtime_columns(columns: &[reg::ColumnDef]) -> String {
    if columns.is_empty() {
        return "[]".to_string();
    }
    let parts: Vec<String> = columns
        .iter()
        .map(|col| format!("{}:{}", col.name, describe_runtime_logicaltype(&col.logical)))
        .collect();
    format!("[{}]", parts.join(", "))
}

pub fn summarize_funcopts(options: Option<&reg::FuncOpts>) -> String {
    match options {
        None => "none".to_string(),
        Some(opts) => {
            let description = opts.description.as_ref().map(|s| s.as_str()).unwrap_or("-");
            let tags = if opts.tags.is_empty() {
                "none".to_string()
            } else {
                format!("[{}]", opts.tags.join(", "))
            };
            let attrs = opts.attributes.describe();
            format!("description='{description}', tags={tags}, attrs={attrs}")
        }
    }
}

pub fn summarize_extopts(options: Option<&reg::ExtOpts>) -> String {
    match options {
        None => "none".to_string(),
        Some(opts) => {
            let description = opts.description.as_ref().map(|s| s.as_str()).unwrap_or("-");
            let tags = if opts.tags.is_empty() {
                "none".to_string()
            } else {
                format!("[{}]", opts.tags.join(", "))
            };
            format!("description='{description}', tags={tags}")
        }
    }
}

pub fn describe_runtime_logicaltype(ty: &reg::LogicalType) -> &'static str {
    ty.describe()
}

// ---------------------------------------------------------------------------
// ExtensionInstance
// ---------------------------------------------------------------------------

/// A loaded extension component: its wasmtime store and generated bindings.
/// `dispatch_*` re-enter the guest's `callback-dispatch` export for each
/// DuckDB-side invocation.
pub struct ExtensionInstance {
    store: Store<ExtensionStoreState>,
    bindings: DuckdbExtension,
}

fn map_extension_trap(err: wasmtime::Error) -> extension_types::Duckerror {
    extension_types::Duckerror::Internal(format!("extension trap: {err}"))
}

impl ExtensionInstance {
    pub fn new(store: Store<ExtensionStoreState>, bindings: DuckdbExtension) -> Self {
        Self { store, bindings }
    }

    pub fn dispatch_scalar(
        &mut self,
        dispatcher_handle: u32,
        args: &[extension_types::Duckvalue],
        ctx: extension_runtime::Invokeinfo,
    ) -> Result<extension_types::Duckvalue, extension_types::Duckerror> {
        let guest = self.bindings.duckdb_extension_callback_dispatch();
        let mut store = self.store.as_context_mut();
        guest
            .call_call_scalar(&mut store, dispatcher_handle, args, ctx)
            .map_err(map_extension_trap)?
    }

    #[allow(clippy::ptr_arg)] // the bindgen call takes &Vec (the rowbatch type), not a slice
    pub fn dispatch_scalar_batch(
        &mut self,
        dispatcher_handle: u32,
        rows: &Vec<Vec<extension_types::Duckvalue>>,
        ctx: extension_runtime::Invokeinfo,
    ) -> Result<Vec<extension_types::Duckvalue>, extension_types::Duckerror> {
        let guest = self.bindings.duckdb_extension_callback_dispatch();
        let mut store = self.store.as_context_mut();
        guest
            .call_call_scalar_batch(&mut store, dispatcher_handle, rows, ctx)
            .map_err(map_extension_trap)?
    }

    pub fn dispatch_table(
        &mut self,
        dispatcher_handle: u32,
        args: &[extension_types::Duckvalue],
    ) -> Result<extension_runtime::Resultset, extension_types::Duckerror> {
        let guest = self.bindings.duckdb_extension_callback_dispatch();
        let mut store = self.store.as_context_mut();
        guest
            .call_call_table(&mut store, dispatcher_handle, args)
            .map_err(map_extension_trap)?
    }

    pub fn dispatch_aggregate(
        &mut self,
        dispatcher_handle: u32,
        rows: &extension_runtime::Rowbatch,
    ) -> Result<extension_types::Duckvalue, extension_types::Duckerror> {
        let guest = self.bindings.duckdb_extension_callback_dispatch();
        let mut store = self.store.as_context_mut();
        guest
            .call_call_aggregate(&mut store, dispatcher_handle, rows)
            .map_err(map_extension_trap)?
    }

    pub fn dispatch_pragma(
        &mut self,
        dispatcher_handle: u32,
        args: &[extension_types::Duckvalue],
    ) -> Result<Option<extension_types::Duckvalue>, extension_types::Duckerror> {
        let guest = self.bindings.duckdb_extension_callback_dispatch();
        let mut store = self.store.as_context_mut();
        guest
            .call_call_pragma(&mut store, dispatcher_handle, args)
            .map_err(map_extension_trap)?
    }

    pub fn dispatch_cast(
        &mut self,
        dispatcher_handle: u32,
        value: &extension_types::Duckvalue,
    ) -> Result<extension_types::Duckvalue, extension_types::Duckerror> {
        let guest = self.bindings.duckdb_extension_callback_dispatch();
        let mut store = self.store.as_context_mut();
        guest
            .call_call_cast(&mut store, dispatcher_handle, value)
            .map_err(map_extension_trap)?
    }

    pub fn drain_pending(&mut self) -> PendingRegistrationsData {
        let mut ctx = self.store.as_context_mut();
        let data: *mut ExtensionStoreState = ctx.data_mut();
        unsafe { (*data).drain_pending() }
    }
}

/// Add the full `duckdb:extension` capability surface to `linker`: the wasip2
/// preview interfaces (so the component's WASI imports resolve) plus all six
/// extension interfaces (types, runtime, config, logging, catalog, files), each
/// dispatched to the `ExtensionStoreState`. Used by both directions before
/// instantiating a component.
pub fn add_extension_interfaces_to_linker(
    linker: &mut Linker<ExtensionStoreState>,
) -> wasmtime::Result<()> {
    wasmtime_wasi::p2::add_to_linker_sync(linker)?;
    extension_types::add_to_linker::<ExtensionStoreState, ExtensionStoreState>(linker, |s| s)?;
    extension_runtime::add_to_linker::<ExtensionStoreState, ExtensionStoreState>(linker, |s| s)?;
    extension_config::add_to_linker::<ExtensionStoreState, ExtensionStoreState>(linker, |s| s)?;
    extension_logging::add_to_linker::<ExtensionStoreState, ExtensionStoreState>(linker, |s| s)?;
    extension_catalog::add_to_linker::<ExtensionStoreState, ExtensionStoreState>(linker, |s| s)?;
    extension_files::add_to_linker::<ExtensionStoreState, ExtensionStoreState>(linker, |s| s)?;
    extension_storage::add_to_linker::<ExtensionStoreState, ExtensionStoreState>(linker, |s| s)?;
    Ok(())
}

/// Load a `duckdb:extension` component and run its `load()`, returning the
/// instantiated [`ExtensionInstance`] (which then holds the registrations the
/// component captured into its store-state via the `Host*` impls).
///
/// This is the direction-agnostic loader: the caller supplies the `wasi` context
/// (so it owns the sandbox/network policy) and the [`ExtensionServices`] sink
/// (so config/logging route to its database). Direction 1 (the wasm-DuckDB host)
/// and Direction 2 (the native-DuckDB extension) call this identically; only the
/// `services` they pass differ.
pub fn load_component(
    engine: &Engine,
    component: &Component,
    wasi: WasiCtx,
    services: Box<dyn ExtensionServices>,
    callback_registry: Arc<Mutex<CallbackRegistry>>,
    extension_name: String,
) -> wasmtime::Result<ExtensionInstance> {
    let mut store = Store::new(
        engine,
        ExtensionStoreState::new(wasi, services, callback_registry, extension_name.clone()),
    );
    let mut linker = Linker::<ExtensionStoreState>::new(engine);
    add_extension_interfaces_to_linker(&mut linker)?;

    let instance_pre = linker.instantiate_pre(component)?;
    let pre = DuckdbExtensionPre::new(instance_pre)?;
    let bindings = pre.instantiate(store.as_context_mut())?;
    bindings
        .duckdb_extension_guest()
        .call_load(store.as_context_mut())?
        .map_err(|err| {
            wasmtime::Error::msg(format!(
                "extension component '{extension_name}' returned error from load(): {err:?}"
            ))
        })?;
    Ok(ExtensionInstance::new(store, bindings))
}
