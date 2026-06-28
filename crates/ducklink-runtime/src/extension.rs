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
    arrow_ext as extension_arrow_ext, catalog as extension_catalog,
    compression as extension_compression, config as extension_config,
    coordinate_system as extension_coordinate_system, encoding as extension_encoding,
    files as extension_files, collation as extension_collation, files_reg as extension_files_reg,
    index as extension_index, lifecycle as extension_lifecycle, logging as extension_logging,
    macro_ext as extension_macro_ext, optimizer as extension_optimizer, parser as extension_parser,
    query as extension_query, runtime as extension_runtime,
    runtime_ext as extension_runtime_ext, secret as extension_secret,
    settings as extension_settings, storage as extension_storage,
    table_stream as extension_table_stream, types as extension_types,
    types_ext as extension_types_ext,
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

    /// v1.1 live-query host import (catalog completion). Run `sql` (a read-only
    /// SELECT) on the live database and return the rows as text cells (every cell
    /// stringified; NULL -> ""). BEST-EFFORT: if the core is busy (the call
    /// arrives from inside a query callback, so the executor is already locked /
    /// mid-call) or the SQL fails, return Err(message) and the caller degrades.
    /// The default impl reports unavailability, so directions that don't wire a
    /// live connection (e.g. tests) still compile.
    fn query(&mut self, _sql: &str) -> Result<Vec<Vec<String>>, String> {
        Err("live query not available in this host".to_string())
    }
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
type PendingIndex = reg::IndexReg;
type PendingFiles = reg::FilesReg;
type PendingCollation = reg::CollationReg;
type PendingPragma = reg::PragmaReg;
// 2.1.0 additive captures.
type PendingCopyHandler = reg::CopyHandlerReg;
type PendingSecret = reg::SecretReg;
type PendingSetting = reg::SettingReg;
type PendingTableMacro = reg::TableMacroReg;
type PendingModifiedType = reg::ModifiedTypeReg;
type PendingEnumType = reg::EnumTypeReg;
// 2.2.0 additive captures (Items 6-7).
type PendingScalarEx = reg::ScalarExReg;
type PendingConnCallback = reg::ConnCallbackReg;
type PendingCoordinateSystem = reg::CoordinateSystemReg;
type PendingArrowTable = reg::ArrowTableReg;
type PendingEncoding = reg::EncodingReg;
type PendingCompression = reg::CompressionReg;
// 2.3.0 / v3 additive captures.
type PendingParser = reg::ParserReg;
type PendingOptimizer = reg::OptimizerReg;
// 3.1.0 additive capture: streaming/filter-pushdown table function.
type PendingFilterableTable = reg::FilterableTableReg;

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
    pending_indexes: Vec<PendingIndex>,
    pending_files: Vec<PendingFiles>,
    pending_collations: Vec<PendingCollation>,
    pending_pragmas: Vec<PendingPragma>,
    // 2.1.0 additive capture buffers.
    pending_copy_handlers: Vec<PendingCopyHandler>,
    pending_secrets: Vec<PendingSecret>,
    pending_settings: Vec<PendingSetting>,
    pending_table_macros: Vec<PendingTableMacro>,
    pending_modified_types: Vec<PendingModifiedType>,
    pending_enum_types: Vec<PendingEnumType>,
    // 2.2.0 additive capture buffers (Items 6-7).
    pending_scalar_ex: Vec<PendingScalarEx>,
    pending_conn_callbacks: Vec<PendingConnCallback>,
    pending_coordinate_systems: Vec<PendingCoordinateSystem>,
    pending_arrow_tables: Vec<PendingArrowTable>,
    pending_encodings: Vec<PendingEncoding>,
    pending_compressions: Vec<PendingCompression>,
    // 2.3.0 / v3 additive capture buffers.
    pending_parsers: Vec<PendingParser>,
    pending_optimizers: Vec<PendingOptimizer>,
    pending_filterable_tables: Vec<PendingFilterableTable>,
    /// Maps the handle returned from `table-registry.register` to the table
    /// function name, so `files.register-replacement-scan` can resolve it.
    table_handle_names: HashMap<u32, String>,
    callback_registry: Arc<Mutex<CallbackRegistry>>,
    extension_name: String,
    /// `Some(..)` only for a component that imports `compose:dynlink/linker`
    /// (the gate is in `load_component`); every other extension is unaffected
    /// and pays nothing. The bridge resolves/invokes the shared, resident
    /// provider (e.g. the one warmed ~38 MB pylon) on the guest's behalf.
    dynlink: Option<crate::compose_dynlink::DynLinkBridge>,
}

impl ExtensionStoreState {
    pub fn new(
        wasi: WasiCtx,
        services: Box<dyn ExtensionServices>,
        callback_registry: Arc<Mutex<CallbackRegistry>>,
        extension_name: String,
    ) -> Self {
        Self::with_dynlink(wasi, services, callback_registry, extension_name, None)
    }

    /// Like [`new`](Self::new) but also carries an optional
    /// `compose:dynlink/linker` bridge (for a component that imports it).
    pub fn with_dynlink(
        wasi: WasiCtx,
        services: Box<dyn ExtensionServices>,
        callback_registry: Arc<Mutex<CallbackRegistry>>,
        extension_name: String,
        dynlink: Option<crate::compose_dynlink::DynLinkBridge>,
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
            pending_indexes: Vec::new(),
            pending_files: Vec::new(),
            pending_collations: Vec::new(),
            pending_pragmas: Vec::new(),
            pending_copy_handlers: Vec::new(),
            pending_secrets: Vec::new(),
            pending_settings: Vec::new(),
            pending_table_macros: Vec::new(),
            pending_modified_types: Vec::new(),
            pending_enum_types: Vec::new(),
            pending_scalar_ex: Vec::new(),
            pending_conn_callbacks: Vec::new(),
            pending_coordinate_systems: Vec::new(),
            pending_arrow_tables: Vec::new(),
            pending_encodings: Vec::new(),
            pending_compressions: Vec::new(),
            pending_parsers: Vec::new(),
            pending_optimizers: Vec::new(),
            pending_filterable_tables: Vec::new(),
            table_handle_names: HashMap::new(),
            callback_registry,
            extension_name,
            dynlink,
        }
    }

    /// Accessor for the dynlink bridge, used by `impl_compose_dynlink_host!`.
    /// Reached only after the `imports_linker` gate set `dynlink = Some(..)`,
    /// so the `expect` never fires for a component wired through that gate.
    fn dynlink_bridge(&mut self) -> &mut crate::compose_dynlink::DynLinkBridge {
        self.dynlink
            .as_mut()
            .expect("dynlink bridge present only when the component imports compose:dynlink/linker")
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
            .unwrap_or_else(|e| e.into_inner());
        registry.allocate(&self.extension_name, kind, dispatcher_handle)
    }

    fn release_callback_handle(&self, handle: u32) {
        let mut registry = self
            .callback_registry
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        registry.remove(handle);
    }

    /// Drains ONLY the captured storage-backend registrations, leaving every
    /// other pending registration (scalars/tables/...) intact for the normal
    /// `drain_pending` hook flow. Used right after `load()` so an ATTACH backend
    /// is routable before the core ever drains function registrations.
    fn take_pending_storages(&mut self) -> Vec<PendingStorage> {
        std::mem::take(&mut self.pending_storages)
    }

    /// Item 3 / M2a: drains ONLY the captured custom-index TYPE registrations,
    /// used right after `load()` so the host can surface them to the core (which
    /// pulls the list via `index-host.index-type-list` and registers a wasm
    /// IndexType for each, routing `CREATE INDEX ... USING <type>` to the
    /// component's index-dispatch export).
    fn take_pending_indexes(&mut self) -> Vec<PendingIndex> {
        std::mem::take(&mut self.pending_indexes)
    }

    /// Drains ONLY the captured files-backend registrations (httpfs M2), used
    /// right after `load()` so the host knows which component backs http(s)
    /// reads before any query runs.
    fn take_pending_files(&mut self) -> Vec<PendingFiles> {
        std::mem::take(&mut self.pending_files)
    }

    /// Drains ONLY the captured collation registrations (Item 2), used right
    /// after `load()` so the host can surface them to the core (which pulls the
    /// list via `collation-host.collation-list` and wraps each as a DuckDB
    /// collation reusing the already-registered sort-key scalar).
    fn take_pending_collations(&mut self) -> Vec<PendingCollation> {
        std::mem::take(&mut self.pending_collations)
    }

    /// Item 4: drains ONLY the captured pragma registrations, used right after
    /// `load()` so the host can surface them to the core (which pulls the list
    /// via `pragma-host.pragma-list` and intercepts `PRAGMA <name>(...)`).
    fn take_pending_pragmas(&mut self) -> Vec<PendingPragma> {
        std::mem::take(&mut self.pending_pragmas)
    }

    // --- 2.1.0 additive drains (mirror take_pending_pragmas) ---
    fn take_pending_copy_handlers(&mut self) -> Vec<PendingCopyHandler> {
        std::mem::take(&mut self.pending_copy_handlers)
    }
    fn take_pending_secrets(&mut self) -> Vec<PendingSecret> {
        std::mem::take(&mut self.pending_secrets)
    }
    fn take_pending_settings(&mut self) -> Vec<PendingSetting> {
        std::mem::take(&mut self.pending_settings)
    }
    fn take_pending_table_macros(&mut self) -> Vec<PendingTableMacro> {
        std::mem::take(&mut self.pending_table_macros)
    }
    fn take_pending_modified_types(&mut self) -> Vec<PendingModifiedType> {
        std::mem::take(&mut self.pending_modified_types)
    }
    fn take_pending_enum_types(&mut self) -> Vec<PendingEnumType> {
        std::mem::take(&mut self.pending_enum_types)
    }

    // --- 2.2.0 additive drains (Items 6-7; mirror the 2.1.0 drains) ---
    fn take_pending_scalar_ex(&mut self) -> Vec<PendingScalarEx> {
        std::mem::take(&mut self.pending_scalar_ex)
    }
    fn take_pending_conn_callbacks(&mut self) -> Vec<PendingConnCallback> {
        std::mem::take(&mut self.pending_conn_callbacks)
    }
    fn take_pending_coordinate_systems(&mut self) -> Vec<PendingCoordinateSystem> {
        std::mem::take(&mut self.pending_coordinate_systems)
    }
    fn take_pending_arrow_tables(&mut self) -> Vec<PendingArrowTable> {
        std::mem::take(&mut self.pending_arrow_tables)
    }
    fn take_pending_encodings(&mut self) -> Vec<PendingEncoding> {
        std::mem::take(&mut self.pending_encodings)
    }
    fn take_pending_compressions(&mut self) -> Vec<PendingCompression> {
        std::mem::take(&mut self.pending_compressions)
    }

    // --- 2.3.0 / v3 additive drains ---
    fn take_pending_parsers(&mut self) -> Vec<PendingParser> {
        std::mem::take(&mut self.pending_parsers)
    }
    fn take_pending_optimizers(&mut self) -> Vec<PendingOptimizer> {
        std::mem::take(&mut self.pending_optimizers)
    }
    // --- 3.1.0 additive drain ---
    fn take_pending_filterable_tables(&mut self) -> Vec<PendingFilterableTable> {
        std::mem::take(&mut self.pending_filterable_tables)
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

// Satisfy a guest's `compose:dynlink/linker` import by delegating to the ONE
// bridge implementation (resolve/invoke against the shared, resident provider
// registry). Only components that actually import the linker get the host
// import added (the `imports_linker` gate in `load_component`).
crate::impl_compose_dynlink_host!(ExtensionStoreState, dynlink_bridge);

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
            // Item 4: pragma capability. The PragmaRegistry resource carries no
            // per-registry buffer (register_call captures pragmas directly into
            // pending_pragmas), so just hand back a fresh resource id.
            extension_runtime::Capabilitykind::Pragma => {
                let id = self.alloc_resource_id();
                Some(extension_runtime::Capability::Pragma(
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
            extension_runtime::Capabilitykind::Pragma,
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
                .unwrap_or_else(|e| e.into_inner());
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
                .unwrap_or_else(|e| e.into_inner());
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
                .unwrap_or_else(|e| e.into_inner());
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
    // Item 4: a component declares a PRAGMA in `load()`. The host captures its
    // name + the callback handle into the neutral pending buffer; the core later
    // pulls the list (via `pragma-host.pragma-list`), intercepts
    // `PRAGMA <name>(...)`, dispatches via callback-dispatch.call-pragma (the
    // component RETURNS a SQL script as text), and runs that script.
    fn register_call(
        &mut self,
        _self_: Resource<extension_runtime::PragmaRegistry>,
        name: String,
        _arguments: BindgenVec<extension_runtime::Funcarg>,
        _returns: extension_runtime::Logicaltype,
        callback: Resource<extension_runtime::PragmaCallback>,
        _options: Option<extension_runtime::Extopts>,
    ) -> Result<u32, extension_types::Duckerror> {
        {
            let registry = self
                .callback_registry
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            match registry.get(callback.rep()) {
                Some(entry) if entry.kind == CallbackKind::Pragma => {}
                Some(_) => {
                    return Err(extension_types::Duckerror::Invalidargument(
                        "callback handle is not a pragma".to_string(),
                    ))
                }
                None => {
                    return Err(extension_types::Duckerror::Internal(
                        "unknown pragma callback handle".to_string(),
                    ))
                }
            }
        }

        let callback_handle = callback.rep();
        std::mem::forget(callback);

        eprintln!(
            "[extension-runtime:{}] registered pragma '{name}' (callback={callback_handle})",
            self.extension_name
        );
        self.pending_pragmas.push(PendingPragma {
            extension: self.extension_name.clone(),
            name,
            callback_handle,
        });
        Ok(self.alloc_resource_id())
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
        // 2.1.0 (Item 1): a COPY handler is captured into the neutral pending
        // buffer; COPY TO / COPY FROM are driven through the component's exported
        // `copy-dispatch` (see ExtensionInstance::copy_*). The `function` field is
        // the copy-function-handle the host threads back into every dispatch call.
        let id = self.alloc_resource_id();
        eprintln!(
            "[extension-manager] files register-copy-handler ext='{}' (function={}) for '{}' -> id {id}",
            handler.extension, handler.function, self.extension_name
        );
        self.pending_copy_handlers.push(PendingCopyHandler {
            extension: self.extension_name.clone(),
            file_extension: handler.extension,
            function_handle: handler.function,
        });
        Ok(id)
    }
}

// 2.1.0 (Item 2): the `secret` interface lets a component declare a secret TYPE
// and named PROVIDERs in `load()`. The host satisfies the import so
// secret-capable components instantiate; the declaration is captured into the
// neutral pending buffer. Materializing a concrete secret is driven through the
// component's exported `secret-dispatch`.
impl extension_secret::Host for ExtensionStoreState {
    fn register_secret_type(
        &mut self,
        type_name: String,
        params: BindgenVec<extension_secret::SecretParam>,
        callback_handle: u32,
    ) -> Result<u32, extension_types::Duckerror> {
        eprintln!(
            "[extension-runtime:{}] registered secret type '{type_name}' ({} params, callback={callback_handle})",
            self.extension_name,
            params.len()
        );
        self.pending_secrets.push(PendingSecret {
            extension: self.extension_name.clone(),
            type_name,
            provider: None,
            params: params.into_iter().map(|p| (p.name, p.redacted)).collect(),
            callback_handle,
        });
        Ok(self.alloc_resource_id())
    }

    fn register_secret_provider(
        &mut self,
        type_name: String,
        provider: String,
        callback_handle: u32,
    ) -> Result<u32, extension_types::Duckerror> {
        eprintln!(
            "[extension-runtime:{}] registered secret provider '{type_name}'/'{provider}' (callback={callback_handle})",
            self.extension_name
        );
        self.pending_secrets.push(PendingSecret {
            extension: self.extension_name.clone(),
            type_name,
            provider: Some(provider),
            params: Vec::new(),
            callback_handle,
        });
        Ok(self.alloc_resource_id())
    }
}

// 2.1.0 (Item 3): the `settings` interface lets a component DECLARE a config
// option (distinct from reading config via `config`). Captured into the neutral
// pending buffer; the direction-specific sink surfaces it to the database.
impl extension_settings::Host for ExtensionStoreState {
    fn register_option(
        &mut self,
        name: String,
        description: String,
        ty: extension_settings::SettingType,
        default_value: Option<String>,
        scope: extension_settings::SettingScope,
    ) -> Result<(), extension_types::Duckerror> {
        let ty = match ty {
            extension_settings::SettingType::Boolean => "boolean",
            extension_settings::SettingType::Varchar => "varchar",
            extension_settings::SettingType::Bigint => "bigint",
            extension_settings::SettingType::Double => "double",
        }
        .to_string();
        let scope = match scope {
            extension_settings::SettingScope::Local => "local",
            extension_settings::SettingScope::Global => "global",
        }
        .to_string();
        eprintln!(
            "[extension-runtime:{}] registered option '{name}' (type={ty}, scope={scope})",
            self.extension_name
        );
        self.pending_settings.push(PendingSetting {
            extension: self.extension_name.clone(),
            name,
            description,
            ty,
            default_value,
            scope,
        });
        Ok(())
    }
}

// 2.3.0 / v3: the `parser` interface declares a parser extension. Captured into a
// neutral pending buffer; the core shim drains it and wires a DuckDB
// `ParserExtension` that forwards unrecognized statement text to the component's
// `parser-dispatch.call-parse` and applies the returned string->SQL rewrite.
impl extension_parser::Host for ExtensionStoreState {
    fn register_parser_extension(
        &mut self,
        name: String,
        callback_handle: u32,
    ) -> Result<u32, extension_types::Duckerror> {
        let registry_id = self.alloc_resource_id();
        eprintln!(
            "[extension-runtime:{}] registered parser extension '{name}' (registry={registry_id}, callback={callback_handle})",
            self.extension_name
        );
        self.pending_parsers.push(PendingParser {
            extension: self.extension_name.clone(),
            name,
            callback_handle,
        });
        Ok(registry_id)
    }
}

// 2.3.0 / v3: the `optimizer` interface declares a general optimizer rule.
// Captured into a neutral pending buffer; the core shim drains it and wires a
// DuckDB `OptimizerExtension` that offers the flattened plan-shape to the
// component's `optimizer-dispatch.call-optimize` and applies the rewrite directive.
impl extension_optimizer::Host for ExtensionStoreState {
    fn register_optimizer_rule(
        &mut self,
        rule_name: String,
        callback_handle: u32,
    ) -> Result<u32, extension_types::Duckerror> {
        let registry_id = self.alloc_resource_id();
        eprintln!(
            "[extension-runtime:{}] registered optimizer rule '{rule_name}' (registry={registry_id}, callback={callback_handle})",
            self.extension_name
        );
        self.pending_optimizers.push(PendingOptimizer {
            extension: self.extension_name.clone(),
            rule_name,
            callback_handle,
        });
        Ok(registry_id)
    }
}

// 3.1.0 (the first additive MINOR off the frozen major-3 baseline): the
// `table-stream` interface declares a STREAMING + FILTER-PUSHDOWN-capable table
// function. Captured into a neutral pending buffer; the core shim drains it and
// wires a C++ streaming `TableFunction` with `filter_pushdown = true` that pushes
// the conjunctive filter set down (as a neutral, by-value-safe descriptor) to the
// component's `table-stream-dispatch.call-table-open-filtered` export.
//
// FREEZE-COMPLIANT: this is a brand-new interface (`table-stream`) in a new opt-in
// world; the shared `runtime`/`types` enums are untouched, so every existing
// @3.0.0 component keeps loading un-rebuilt.
impl extension_table_stream::Host for ExtensionStoreState {
    fn register_filterable_table(
        &mut self,
        name: String,
        arguments: BindgenVec<extension_table_stream::Funcarg>,
        columns: BindgenVec<extension_table_stream::Columndef>,
        callback_handle: u32,
    ) -> Result<u32, extension_types::Duckerror> {
        let converted_arguments = convert_extension_funcargs(arguments.into());
        let converted_columns = convert_extension_columndefs(columns.into());
        // Allocate a GLOBALLY-ROUTABLE handle (mapping global -> this extension +
        // the component-local `callback_handle` dispatcher) so the core can carry
        // ONE u32 in the C++ TableFunction and the host routes every streaming
        // dispatch call (open-filtered / next / close) back to the owning
        // component, exactly as the regular table-callback path routes call-table.
        let global = self.allocate_callback_handle(callback_handle, CallbackKind::Table);
        eprintln!(
            "[extension-runtime:{}] registered filterable streaming table fn '{name}' (global={global}, dispatcher={callback_handle}, args={}, cols={})",
            self.extension_name,
            converted_arguments.len(),
            converted_columns.len(),
        );
        self.pending_filterable_tables.push(PendingFilterableTable {
            extension: self.extension_name.clone(),
            name,
            arguments: converted_arguments,
            columns: converted_columns,
            callback_handle: global,
        });
        Ok(global)
    }
}

// 2.1.0 (Item 5): the `macro-ext` interface adds TABLE macros (a relation body)
// on top of the existing scalar-macro registration.
impl extension_macro_ext::Host for ExtensionStoreState {
    fn register_table_macro(
        &mut self,
        schema: String,
        name: String,
        parameters: BindgenVec<String>,
        body_sql: String,
    ) -> Result<(), extension_types::Duckerror> {
        eprintln!(
            "[extension-runtime:{}] registered table macro '{schema}.{name}' ({} params)",
            self.extension_name,
            parameters.len()
        );
        self.pending_table_macros.push(PendingTableMacro {
            extension: self.extension_name.clone(),
            schema,
            name,
            parameters: parameters.into_iter().collect(),
            body_sql,
        });
        Ok(())
    }
}

// 2.1.0 (Item 5): the `types-ext` interface adds modified logical types (over a
// type-expression, riding the escape hatch) and ENUM types. `types` stays FROZEN.
impl extension_types_ext::Host for ExtensionStoreState {
    fn register_logical_type_modified(
        &mut self,
        name: String,
        type_expr: String,
    ) -> Result<u32, extension_types::Duckerror> {
        eprintln!(
            "[extension-runtime:{}] registered modified logical type '{name}' = {type_expr}",
            self.extension_name
        );
        self.pending_modified_types.push(PendingModifiedType {
            extension: self.extension_name.clone(),
            name,
            type_expr,
        });
        Ok(self.alloc_resource_id())
    }

    fn register_enum(
        &mut self,
        name: String,
        members: BindgenVec<String>,
    ) -> Result<u32, extension_types::Duckerror> {
        eprintln!(
            "[extension-runtime:{}] registered enum type '{name}' ({} members)",
            self.extension_name,
            members.len()
        );
        self.pending_enum_types.push(PendingEnumType {
            extension: self.extension_name.clone(),
            name,
            members: members.into_iter().collect(),
        });
        Ok(self.alloc_resource_id())
    }
}

// 2.2.0 (Item 6): the `runtime-ext` interface adds a RICHER scalar registration
// (varargs + named args + NULL handling) without touching the frozen `runtime`
// scalar-registry signature. Captured into the neutral pending buffer; the
// direction-specific sink forwards it. A callback handle is allocated exactly
// like the base scalar path so invocations route to the owning component.
impl extension_runtime_ext::Host for ExtensionStoreState {
    fn register_scalar_ex(
        &mut self,
        name: String,
        arguments: BindgenVec<extension_runtime_ext::Funcarg>,
        varargs: Option<extension_runtime_ext::Logicaltype>,
        returns: extension_runtime_ext::Logicaltype,
        null_handling: extension_runtime_ext::NullHandling,
        callback_handle: u32,
        options: Option<extension_runtime_ext::Funcopts>,
    ) -> Result<u32, extension_types::Duckerror> {
        let special_null = matches!(null_handling, extension_runtime_ext::NullHandling::Special);
        let arguments = convert_extension_funcargs(arguments.into_iter().collect());
        let varargs = varargs.map(convert_extension_logicaltype);
        let returns = convert_extension_logicaltype(returns);
        let options = options.map(convert_extension_funcopts);
        let registry_id = self.alloc_resource_id();
        eprintln!(
            "[extension-runtime:{}] registered scalar-ex '{name}' (registry={registry_id}, callback={callback_handle}, varargs={}, special_null={special_null})",
            self.extension_name,
            varargs.is_some()
        );
        self.pending_scalar_ex.push(PendingScalarEx {
            extension: self.extension_name.clone(),
            name,
            arguments,
            varargs,
            returns,
            special_null,
            callback_handle,
            options,
        });
        Ok(registry_id)
    }
}

// 2.2.0 (Item 7): the `lifecycle` interface lets a component subscribe to
// connection open/close events; the host captures the subscription and drives the
// notifications through the separate `conn-dispatch` export.
impl extension_lifecycle::Host for ExtensionStoreState {
    fn register_connection_callback(
        &mut self,
        events: extension_lifecycle::ConnEvents,
        callback_handle: u32,
    ) -> Result<u32, extension_types::Duckerror> {
        let on_opened = events.contains(extension_lifecycle::ConnEvents::OPENED);
        let on_closed = events.contains(extension_lifecycle::ConnEvents::CLOSED);
        eprintln!(
            "[extension-runtime:{}] registered connection callback (opened={on_opened}, closed={on_closed}, callback={callback_handle})",
            self.extension_name
        );
        self.pending_conn_callbacks.push(PendingConnCallback {
            extension: self.extension_name.clone(),
            on_opened,
            on_closed,
            callback_handle,
        });
        Ok(self.alloc_resource_id())
    }
}

// 2.2.0 (Item 7): the `coordinate-system` interface lets a spatial component
// declare CRS definitions (authority + code + WKT2) in load(); the host captures
// them so the core can resolve geometry SRIDs. Registration only -- reprojection
// (GDAL/PROJ ST_Transform) is OUT OF SCOPE for 2.2.0.
impl extension_coordinate_system::Host for ExtensionStoreState {
    fn register_coordinate_system(
        &mut self,
        crs: extension_coordinate_system::CrsDef,
    ) -> Result<u32, extension_types::Duckerror> {
        eprintln!(
            "[extension-runtime:{}] registered coordinate system {}:{}",
            self.extension_name, crs.auth_name, crs.code
        );
        self.pending_coordinate_systems.push(PendingCoordinateSystem {
            extension: self.extension_name.clone(),
            auth_name: crs.auth_name,
            code: crs.code,
            wkt: crs.wkt,
        });
        Ok(self.alloc_resource_id())
    }
}

// 2.2.0 (Item 7): the `arrow-ext` interface lets a component declare an Arrow
// table producer; the host captures the declaration and streams the batches via
// the producer's callback handle (reusing the table cursor shape).
impl extension_arrow_ext::Host for ExtensionStoreState {
    fn register_arrow_table(
        &mut self,
        name: String,
        schema: BindgenVec<extension_arrow_ext::Columndef>,
        callback_handle: u32,
    ) -> Result<u32, extension_types::Duckerror> {
        let columns = convert_extension_columndefs(schema.into_iter().collect());
        eprintln!(
            "[extension-runtime:{}] registered arrow table '{name}' ({} columns, callback={callback_handle})",
            self.extension_name,
            columns.len()
        );
        self.pending_arrow_tables.push(PendingArrowTable {
            extension: self.extension_name.clone(),
            name,
            columns,
            callback_handle,
        });
        Ok(self.alloc_resource_id())
    }
}

// 2.2.0 (Item 7): the `encoding` interface lets a component declare a text
// encoding it can transcode to UTF-8; the host captures the declaration so the
// CSV/text readers can route an `encoding=` option. Transcoding rides an
// already-registered scalar, so no new dispatch export is needed.
impl extension_encoding::Host for ExtensionStoreState {
    fn register_encoding(
        &mut self,
        name: String,
        aliases: BindgenVec<String>,
        callback_handle: u32,
    ) -> Result<u32, extension_types::Duckerror> {
        eprintln!(
            "[extension-runtime:{}] registered text encoding '{name}' ({} aliases, callback={callback_handle})",
            self.extension_name,
            aliases.len()
        );
        self.pending_encodings.push(PendingEncoding {
            extension: self.extension_name.clone(),
            name,
            aliases: aliases.into_iter().collect(),
            callback_handle,
        });
        Ok(self.alloc_resource_id())
    }
}

// 2.2.0 (Item 7): the `compression` interface lets a component declare a
// compression codec keyed by a file extension; the host captures the declaration
// so the file readers/writers can route a matching file. The (de)compression
// rides an already-registered scalar, so no new dispatch export is needed.
impl extension_compression::Host for ExtensionStoreState {
    fn register_compression(
        &mut self,
        name: String,
        file_extension: String,
        callback_handle: u32,
    ) -> Result<u32, extension_types::Duckerror> {
        eprintln!(
            "[extension-runtime:{}] registered compression codec '{name}' (.{file_extension}, callback={callback_handle})",
            self.extension_name
        );
        self.pending_compressions.push(PendingCompression {
            extension: self.extension_name.clone(),
            name,
            file_extension,
            callback_handle,
        });
        Ok(self.alloc_resource_id())
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

// Item 3 / M2a: the `index` interface lets a component register a custom INDEX
// TYPE (e.g. "wasm_hnsw") in `load()`. The host satisfies the import so
// index-capable components instantiate and load; the registration is captured
// into the neutral pending buffer. Driving the component's `index-dispatch`
// export (create/append/build/search/drop) is the direction-specific sink's job.
impl extension_index::Host for ExtensionStoreState {
    fn register_index_type(
        &mut self,
        type_name: String,
    ) -> Result<(), extension_types::Duckerror> {
        eprintln!(
            "[extension-runtime:{}] registered custom index type '{type_name}'",
            self.extension_name
        );
        self.pending_indexes.push(PendingIndex {
            extension: self.extension_name.clone(),
            type_name,
        });
        Ok(())
    }
}

// httpfs M2: the `files-reg` interface lets a component declare itself the files
// backend (an http(s) fetcher) in `load()`. The host satisfies the import so
// files-capable components instantiate; the registration is captured into the
// neutral pending buffer and driving the component's `file-dispatch` export is
// the direction-specific sink's job.
impl extension_files_reg::Host for ExtensionStoreState {
    fn register_files(
        &mut self,
        callback_handle: u32,
    ) -> Result<u32, extension_types::Duckerror> {
        eprintln!(
            "[extension-runtime:{}] registered files backend (callback={callback_handle})",
            self.extension_name
        );
        self.pending_files.push(PendingFiles {
            extension: self.extension_name.clone(),
            callback_handle,
        });
        Ok(self.alloc_resource_id())
    }
}

// Item 2: the `collation` interface lets a component declare a collation in
// `load()` whose transform is an already-registered sort-key scalar. The host
// satisfies the import so collation-capable components (e.g. icufns) instantiate
// and load; the registration is captured into the neutral pending buffer. The
// core later pulls the list (via `collation-host.collation-list`) and wraps each
// as a DuckDB collation reusing the named scalar -- no new dispatch.
impl extension_collation::Host for ExtensionStoreState {
    fn register_collation(
        &mut self,
        name: String,
        transform_scalar: String,
        combinable: bool,
    ) -> Result<(), extension_types::Duckerror> {
        eprintln!(
            "[extension-runtime:{}] registered collation '{name}' (transform scalar='{transform_scalar}', combinable={combinable})",
            self.extension_name
        );
        self.pending_collations.push(PendingCollation {
            extension: self.extension_name.clone(),
            name,
            transform_scalar,
            combinable,
        });
        Ok(())
    }
}

// v1.1: the `query` interface lets a component run a read-only SELECT against the
// live database (catalog completion). The host satisfies the import here by
// forwarding to the direction-specific `ExtensionServices::query` sink. The call
// is BEST-EFFORT: a re-entrant call (from inside a query callback) or a SQL error
// returns Err, which the component treats as "no rows".
impl extension_query::Host for ExtensionStoreState {
    fn query(&mut self, sql: String) -> Result<BindgenVec<BindgenVec<String>>, String> {
        let rows = self.services.query(&sql)?;
        Ok(rows
            .into_iter()
            .map(|row| {
                row.into_iter()
                    .map(Into::into)
                    .collect::<BindgenVec<String>>()
            })
            .collect())
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
        extension_runtime::Logicaltype::Int32 => reg::LogicalType::Int32,
        extension_runtime::Logicaltype::Timestamp => reg::LogicalType::Timestamp,
        extension_runtime::Logicaltype::Int8 => reg::LogicalType::Int8,
        extension_runtime::Logicaltype::Int16 => reg::LogicalType::Int16,
        extension_runtime::Logicaltype::Uint8 => reg::LogicalType::Uint8,
        extension_runtime::Logicaltype::Uint16 => reg::LogicalType::Uint16,
        extension_runtime::Logicaltype::Uint32 => reg::LogicalType::Uint32,
        extension_runtime::Logicaltype::Float32 => reg::LogicalType::Float32,
        extension_runtime::Logicaltype::Date => reg::LogicalType::Date,
        extension_runtime::Logicaltype::Time => reg::LogicalType::Time,
        extension_runtime::Logicaltype::Timestamptz => reg::LogicalType::Timestamptz,
        extension_runtime::Logicaltype::Decimal => reg::LogicalType::Decimal,
        extension_runtime::Logicaltype::Interval => reg::LogicalType::Interval,
        extension_runtime::Logicaltype::Uuid => reg::LogicalType::Uuid,
        extension_runtime::Logicaltype::Complex(expr) => reg::LogicalType::Complex(expr),
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

pub fn describe_runtime_logicaltype(ty: &reg::LogicalType) -> String {
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
    // Raw component instance, retained so the storage-capable bindings can be
    // built on demand for storage backend components (which export
    // storage-dispatch on top of the base world).
    instance: wasmtime::component::Instance,
    // Lazily-built storage bindings (None until first storage-dispatch call or
    // for non-storage extensions).
    storage_bindings: Option<crate::duckdb_extension_storage_bindings::DuckdbExtensionStorage>,
    // Item 3 / M2a: lazily-built index bindings (None until first index-dispatch
    // call or for non-index extensions).
    index_bindings: Option<crate::duckdb_extension_index_bindings::DuckdbExtensionIndex>,
    // httpfs M2: lazily-built files bindings (None until first file-dispatch
    // call or for non-files extensions).
    files_bindings: Option<crate::duckdb_extension_files_bindings::DuckdbExtensionFiles>,
    // 2.3.0 / v3: lazily-built parser-dispatch bindings (None until first
    // call-parse, or for non-parser extensions).
    parser_bindings: Option<crate::duckdb_extension_parser_bindings::DuckdbExtensionParser>,
    // 2.3.0 / v3: lazily-built optimizer-dispatch bindings (None until first
    // call-optimize, or for non-optimizer extensions).
    optimizer_bindings:
        Option<crate::duckdb_extension_optimizer_bindings::DuckdbExtensionOptimizer>,
    // 2.1.0: lazily-built copy / secret / writable-storage bindings.
    copy_bindings: Option<crate::duckdb_extension_copy_bindings::DuckdbExtensionCopy>,
    secret_bindings: Option<crate::duckdb_extension_secret_bindings::DuckdbExtensionSecret>,
    storage_write_bindings:
        Option<crate::duckdb_extension_storage_write_bindings::DuckdbExtensionStorageWrite>,
    // 2.2.0 (Items 6-7): lazily-built dispatch bindings for the additive
    // dispatch-only worlds. Each is built on first use from the SAME loaded
    // component instance, exactly like the 2.1.0 copy/secret/storage-write path.
    table_stream_bindings:
        Option<crate::duckdb_extension_table_stream_bindings::DuckdbExtensionTableStream>,
    aggregate_incr_bindings:
        Option<crate::duckdb_extension_aggregate_incr_bindings::DuckdbExtensionAggregateIncr>,
    conn_bindings: Option<crate::duckdb_extension_conn_bindings::DuckdbExtensionConn>,
    file_write_bindings:
        Option<crate::duckdb_extension_file_write_bindings::DuckdbExtensionFileWrite>,
    index_write_bindings:
        Option<crate::duckdb_extension_index_write_bindings::DuckdbExtensionIndexWrite>,
    settings_bindings: Option<crate::duckdb_extension_settings_bindings::DuckdbExtensionSettings>,
}

fn map_extension_trap(err: wasmtime::Error) -> extension_types::Duckerror {
    extension_types::Duckerror::Internal(format!("extension trap: {err}"))
}

// The storage-capable bindgen world generates its OWN (structurally identical)
// `types`; convert those into the base `extension_types` the rest of the runtime
// uses.
mod storage_types {
    pub use crate::duckdb_extension_storage_bindings::duckdb::extension::types::*;
}

// M2b: the storage interface's scan types (scan-request / scan-filter /
// compare-op) used when driving a pushdown scan into the component.
pub mod storage_scan {
    pub use crate::duckdb_extension_storage_bindings::duckdb::extension::storage::*;
    // The scan-filter `value` field is the storage world's own `types.duckvalue`;
    // re-export it (and the composite record types it carries) so the host can
    // construct scan requests.
    pub use crate::duckdb_extension_storage_bindings::duckdb::extension::types::{
        Complexvalue, Decimalvalue, Duckvalue, Intervalvalue, Uuidvalue,
    };
}

fn storage_duckvalue_to_ext(value: storage_types::Duckvalue) -> extension_types::Duckvalue {
    match value {
        storage_types::Duckvalue::Null => extension_types::Duckvalue::Null,
        storage_types::Duckvalue::Boolean(v) => extension_types::Duckvalue::Boolean(v),
        storage_types::Duckvalue::Int64(v) => extension_types::Duckvalue::Int64(v),
        storage_types::Duckvalue::Uint64(v) => extension_types::Duckvalue::Uint64(v),
        storage_types::Duckvalue::Float64(v) => extension_types::Duckvalue::Float64(v),
        storage_types::Duckvalue::Text(v) => extension_types::Duckvalue::Text(v),
        storage_types::Duckvalue::Blob(v) => extension_types::Duckvalue::Blob(v),
        storage_types::Duckvalue::Int32(v) => extension_types::Duckvalue::Int32(v),
        storage_types::Duckvalue::Timestamp(v) => extension_types::Duckvalue::Timestamp(v),
        storage_types::Duckvalue::Int8(v) => extension_types::Duckvalue::Int8(v),
        storage_types::Duckvalue::Int16(v) => extension_types::Duckvalue::Int16(v),
        storage_types::Duckvalue::Uint8(v) => extension_types::Duckvalue::Uint8(v),
        storage_types::Duckvalue::Uint16(v) => extension_types::Duckvalue::Uint16(v),
        storage_types::Duckvalue::Uint32(v) => extension_types::Duckvalue::Uint32(v),
        storage_types::Duckvalue::Float32(v) => extension_types::Duckvalue::Float32(v),
        storage_types::Duckvalue::Date(v) => extension_types::Duckvalue::Date(v),
        storage_types::Duckvalue::Time(v) => extension_types::Duckvalue::Time(v),
        storage_types::Duckvalue::Timestamptz(v) => extension_types::Duckvalue::Timestamptz(v),
        storage_types::Duckvalue::Decimal(d) => {
            extension_types::Duckvalue::Decimal(extension_types::Decimalvalue {
                lower: d.lower,
                upper: d.upper,
                width: d.width,
                scale: d.scale,
            })
        }
        storage_types::Duckvalue::Interval(iv) => {
            extension_types::Duckvalue::Interval(extension_types::Intervalvalue {
                months: iv.months,
                days: iv.days,
                micros: iv.micros,
            })
        }
        storage_types::Duckvalue::Uuid(u) => {
            extension_types::Duckvalue::Uuid(extension_types::Uuidvalue { hi: u.hi, lo: u.lo })
        }
        storage_types::Duckvalue::Complex(c) => {
            extension_types::Duckvalue::Complex(extension_types::Complexvalue {
                type_expr: c.type_expr,
                json: c.json,
            })
        }
    }
}

fn storage_duckerror_to_ext(err: storage_types::Duckerror) -> extension_types::Duckerror {
    match err {
        storage_types::Duckerror::Invalidargument(m) => extension_types::Duckerror::Invalidargument(m),
        storage_types::Duckerror::Unsupported(m) => extension_types::Duckerror::Unsupported(m),
        storage_types::Duckerror::Invalidstate(m) => extension_types::Duckerror::Invalidstate(m),
        storage_types::Duckerror::Io(m) => extension_types::Duckerror::Io(m),
        storage_types::Duckerror::Internal(m) => extension_types::Duckerror::Internal(m),
    }
}

fn storage_logicaltype_to_ext(ty: storage_types::Logicaltype) -> extension_types::Logicaltype {
    match ty {
        storage_types::Logicaltype::Boolean => extension_types::Logicaltype::Boolean,
        storage_types::Logicaltype::Int64 => extension_types::Logicaltype::Int64,
        storage_types::Logicaltype::Uint64 => extension_types::Logicaltype::Uint64,
        storage_types::Logicaltype::Float64 => extension_types::Logicaltype::Float64,
        storage_types::Logicaltype::Text => extension_types::Logicaltype::Text,
        storage_types::Logicaltype::Blob => extension_types::Logicaltype::Blob,
        storage_types::Logicaltype::Int32 => extension_types::Logicaltype::Int32,
        storage_types::Logicaltype::Timestamp => extension_types::Logicaltype::Timestamp,
        storage_types::Logicaltype::Int8 => extension_types::Logicaltype::Int8,
        storage_types::Logicaltype::Int16 => extension_types::Logicaltype::Int16,
        storage_types::Logicaltype::Uint8 => extension_types::Logicaltype::Uint8,
        storage_types::Logicaltype::Uint16 => extension_types::Logicaltype::Uint16,
        storage_types::Logicaltype::Uint32 => extension_types::Logicaltype::Uint32,
        storage_types::Logicaltype::Float32 => extension_types::Logicaltype::Float32,
        storage_types::Logicaltype::Date => extension_types::Logicaltype::Date,
        storage_types::Logicaltype::Time => extension_types::Logicaltype::Time,
        storage_types::Logicaltype::Timestamptz => extension_types::Logicaltype::Timestamptz,
        storage_types::Logicaltype::Decimal => extension_types::Logicaltype::Decimal,
        storage_types::Logicaltype::Interval => extension_types::Logicaltype::Interval,
        storage_types::Logicaltype::Uuid => extension_types::Logicaltype::Uuid,
        storage_types::Logicaltype::Complex(expr) => extension_types::Logicaltype::Complex(expr),
    }
}

fn storage_columndef_to_ext(col: storage_types::Columndef) -> extension_types::Columndef {
    extension_types::Columndef {
        name: col.name,
        logical: storage_logicaltype_to_ext(col.logical),
    }
}

// Item 3 / M2a: the index-capable bindgen world generates its OWN (structurally
// identical) `types`; convert those into the base `extension_types`.
mod index_types {
    pub use crate::duckdb_extension_index_bindings::duckdb::extension::types::*;
}

/// An index-dispatch nearest-neighbour hit (rowid + distance), re-exported for
/// the host to surface up the index-host import.
pub use crate::duckdb_extension_index_bindings::exports::duckdb::extension::index_dispatch::IndexHit;

/// 2.1.0 (Item 1): result of binding a COPY FROM reader (reader handle +
/// columns), re-exported for the host. `columns` is the base `extension_types`
/// Columndef (the world's `types` is remapped to the base bindings).
pub use crate::duckdb_extension_copy_bindings::exports::duckdb::extension::copy_dispatch::CopyFromBindResult;

/// 2.1.0 (Item 2): one flat key=value entry of a materialized secret,
/// re-exported for the host.
pub use crate::duckdb_extension_secret_bindings::exports::duckdb::extension::secret_dispatch::SecretKv;

/// 2.2.0 (Item 6): result of opening a streaming table cursor (cursor handle +
/// projected column schema), re-exported for the host.
pub use crate::duckdb_extension_table_stream_bindings::exports::duckdb::extension::table_stream_dispatch::TableOpenResult;

/// 3.1.0: the neutral, by-value-safe pushed-down filter descriptor + its
/// comparator enum (`table-stream-dispatch.table-filter` / `filter-op`),
/// re-exported so the core<->host bridge can build the conjunctive filter set
/// the streaming `TableFunction` pushes to `call-table-open-filtered`.
pub use crate::duckdb_extension_table_stream_bindings::exports::duckdb::extension::table_stream_dispatch::{
    FilterOp, TableFilter,
};

/// 2.2.0 (Item 7): metadata for one path returned by `file-write-dispatch.file-stat`,
/// re-exported for the host.
pub use crate::duckdb_extension_file_write_bindings::exports::duckdb::extension::file_write_dispatch::FileInfo;

fn index_duckerror_to_ext(err: index_types::Duckerror) -> extension_types::Duckerror {
    match err {
        index_types::Duckerror::Invalidargument(m) => extension_types::Duckerror::Invalidargument(m),
        index_types::Duckerror::Unsupported(m) => extension_types::Duckerror::Unsupported(m),
        index_types::Duckerror::Invalidstate(m) => extension_types::Duckerror::Invalidstate(m),
        index_types::Duckerror::Io(m) => extension_types::Duckerror::Io(m),
        index_types::Duckerror::Internal(m) => extension_types::Duckerror::Internal(m),
    }
}

impl ExtensionInstance {
    pub fn new(
        store: Store<ExtensionStoreState>,
        bindings: DuckdbExtension,
        instance: wasmtime::component::Instance,
    ) -> Self {
        Self {
            store,
            bindings,
            instance,
            storage_bindings: None,
            index_bindings: None,
            files_bindings: None,
            parser_bindings: None,
            optimizer_bindings: None,
            copy_bindings: None,
            secret_bindings: None,
            storage_write_bindings: None,
            table_stream_bindings: None,
            aggregate_incr_bindings: None,
            conn_bindings: None,
            file_write_bindings: None,
            index_write_bindings: None,
            settings_bindings: None,
        }
    }

    /// Builds (once) the storage-capable bindings from the raw instance. Errors
    /// if this component does not export storage-dispatch (i.e. is not a storage
    /// backend).
    fn storage_bindings(
        &mut self,
    ) -> Result<
        &crate::duckdb_extension_storage_bindings::DuckdbExtensionStorage,
        extension_types::Duckerror,
    > {
        if self.storage_bindings.is_none() {
            let built = crate::duckdb_extension_storage_bindings::DuckdbExtensionStorage::new(
                self.store.as_context_mut(),
                &self.instance,
            )
            .map_err(map_extension_trap)?;
            self.storage_bindings = Some(built);
        }
        Ok(self.storage_bindings.as_ref().unwrap())
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

    /// Drains only the captured storage-backend registrations (see
    /// `ExtensionStoreState::take_pending_storages`).
    pub fn take_pending_storages(&mut self) -> Vec<crate::reg::StorageReg> {
        let mut ctx = self.store.as_context_mut();
        let data: *mut ExtensionStoreState = ctx.data_mut();
        unsafe { (*data).take_pending_storages() }
    }

    /// Item 3 / M2a: drains the captured custom-index TYPE registrations (see
    /// `ExtensionStoreState::take_pending_indexes`).
    pub fn take_pending_indexes(&mut self) -> Vec<crate::reg::IndexReg> {
        let mut ctx = self.store.as_context_mut();
        let data: *mut ExtensionStoreState = ctx.data_mut();
        unsafe { (*data).take_pending_indexes() }
    }

    /// httpfs M2: drains the captured files-backend registrations (see
    /// `ExtensionStoreState::take_pending_files`).
    pub fn take_pending_files(&mut self) -> Vec<crate::reg::FilesReg> {
        let mut ctx = self.store.as_context_mut();
        let data: *mut ExtensionStoreState = ctx.data_mut();
        unsafe { (*data).take_pending_files() }
    }

    /// Item 2: drains the captured collation registrations (see
    /// `ExtensionStoreState::take_pending_collations`).
    pub fn take_pending_collations(&mut self) -> Vec<crate::reg::CollationReg> {
        let mut ctx = self.store.as_context_mut();
        let data: *mut ExtensionStoreState = ctx.data_mut();
        unsafe { (*data).take_pending_collations() }
    }

    /// Item 4: drains the captured pragma registrations (see
    /// `ExtensionStoreState::take_pending_pragmas`).
    pub fn take_pending_pragmas(&mut self) -> Vec<crate::reg::PragmaReg> {
        let mut ctx = self.store.as_context_mut();
        let data: *mut ExtensionStoreState = ctx.data_mut();
        unsafe { (*data).take_pending_pragmas() }
    }

    // --- 2.1.0 additive drains (mirror take_pending_pragmas) ---

    /// 2.1.0 (Item 1): drains the captured COPY-handler registrations.
    pub fn take_pending_copy_handlers(&mut self) -> Vec<crate::reg::CopyHandlerReg> {
        let mut ctx = self.store.as_context_mut();
        let data: *mut ExtensionStoreState = ctx.data_mut();
        unsafe { (*data).take_pending_copy_handlers() }
    }

    /// 2.1.0 (Item 2): drains the captured secret type/provider registrations.
    pub fn take_pending_secrets(&mut self) -> Vec<crate::reg::SecretReg> {
        let mut ctx = self.store.as_context_mut();
        let data: *mut ExtensionStoreState = ctx.data_mut();
        unsafe { (*data).take_pending_secrets() }
    }

    /// 2.1.0 (Item 3): drains the captured option/settings registrations.
    pub fn take_pending_settings(&mut self) -> Vec<crate::reg::SettingReg> {
        let mut ctx = self.store.as_context_mut();
        let data: *mut ExtensionStoreState = ctx.data_mut();
        unsafe { (*data).take_pending_settings() }
    }

    /// 2.1.0 (Item 5): drains the captured table-macro registrations.
    pub fn take_pending_table_macros(&mut self) -> Vec<crate::reg::TableMacroReg> {
        let mut ctx = self.store.as_context_mut();
        let data: *mut ExtensionStoreState = ctx.data_mut();
        unsafe { (*data).take_pending_table_macros() }
    }

    /// 2.1.0 (Item 5): drains the captured modified-logical-type registrations.
    pub fn take_pending_modified_types(&mut self) -> Vec<crate::reg::ModifiedTypeReg> {
        let mut ctx = self.store.as_context_mut();
        let data: *mut ExtensionStoreState = ctx.data_mut();
        unsafe { (*data).take_pending_modified_types() }
    }

    /// 2.1.0 (Item 5): drains the captured ENUM-type registrations.
    pub fn take_pending_enum_types(&mut self) -> Vec<crate::reg::EnumTypeReg> {
        let mut ctx = self.store.as_context_mut();
        let data: *mut ExtensionStoreState = ctx.data_mut();
        unsafe { (*data).take_pending_enum_types() }
    }

    // --- 2.2.0 additive drains (Items 6-7; mirror the 2.1.0 drains) ---

    /// 2.2.0 (Item 6): drains the captured richer scalar (scalar-ex) registrations.
    pub fn take_pending_scalar_ex(&mut self) -> Vec<crate::reg::ScalarExReg> {
        let mut ctx = self.store.as_context_mut();
        let data: *mut ExtensionStoreState = ctx.data_mut();
        unsafe { (*data).take_pending_scalar_ex() }
    }

    /// 2.2.0 (Item 7): drains the captured connection-lifecycle subscriptions.
    pub fn take_pending_conn_callbacks(&mut self) -> Vec<crate::reg::ConnCallbackReg> {
        let mut ctx = self.store.as_context_mut();
        let data: *mut ExtensionStoreState = ctx.data_mut();
        unsafe { (*data).take_pending_conn_callbacks() }
    }

    /// 2.2.0 (Item 7): drains the captured coordinate-system (CRS) registrations.
    pub fn take_pending_coordinate_systems(&mut self) -> Vec<crate::reg::CoordinateSystemReg> {
        let mut ctx = self.store.as_context_mut();
        let data: *mut ExtensionStoreState = ctx.data_mut();
        unsafe { (*data).take_pending_coordinate_systems() }
    }

    /// 2.2.0 (Item 7): drains the captured Arrow-table-producer registrations.
    pub fn take_pending_arrow_tables(&mut self) -> Vec<crate::reg::ArrowTableReg> {
        let mut ctx = self.store.as_context_mut();
        let data: *mut ExtensionStoreState = ctx.data_mut();
        unsafe { (*data).take_pending_arrow_tables() }
    }

    /// 2.2.0 (Item 7): drains the captured text-encoding registrations.
    pub fn take_pending_encodings(&mut self) -> Vec<crate::reg::EncodingReg> {
        let mut ctx = self.store.as_context_mut();
        let data: *mut ExtensionStoreState = ctx.data_mut();
        unsafe { (*data).take_pending_encodings() }
    }

    /// 2.2.0 (Item 7): drains the captured compression-codec registrations.
    pub fn take_pending_compressions(&mut self) -> Vec<crate::reg::CompressionReg> {
        let mut ctx = self.store.as_context_mut();
        let data: *mut ExtensionStoreState = ctx.data_mut();
        unsafe { (*data).take_pending_compressions() }
    }

    /// 2.3.0 / v3: drains the captured parser-extension registrations. The core
    /// shim wires each into a DuckDB `ParserExtension`.
    pub fn take_pending_parsers(&mut self) -> Vec<crate::reg::ParserReg> {
        let mut ctx = self.store.as_context_mut();
        let data: *mut ExtensionStoreState = ctx.data_mut();
        unsafe { (*data).take_pending_parsers() }
    }

    /// 2.3.0 / v3: drains the captured optimizer-rule registrations. The core shim
    /// wires each into a DuckDB `OptimizerExtension`.
    pub fn take_pending_optimizers(&mut self) -> Vec<crate::reg::OptimizerReg> {
        let mut ctx = self.store.as_context_mut();
        let data: *mut ExtensionStoreState = ctx.data_mut();
        unsafe { (*data).take_pending_optimizers() }
    }

    /// 3.1.0: drains the captured streaming/filter-pushdown table-fn registrations
    /// (the first additive MINOR off the frozen major-3 baseline). The core shim
    /// wires each into a C++ streaming `TableFunction` with `filter_pushdown = true`
    /// that drives the component's `table-stream-dispatch.call-table-open-filtered`.
    pub fn take_pending_filterable_tables(&mut self) -> Vec<crate::reg::FilterableTableReg> {
        let mut ctx = self.store.as_context_mut();
        let data: *mut ExtensionStoreState = ctx.data_mut();
        unsafe { (*data).take_pending_filterable_tables() }
    }

    // --- 2.1.0 (Item 1): copy-dispatch re-entry ---
    // Drives a registered COPY handler's exported `copy-dispatch`. Types are
    // remapped to the base `extension_types` (see lib.rs `with`), so no per-world
    // conversion is needed.

    fn copy_bindings(
        &mut self,
    ) -> Result<
        &crate::duckdb_extension_copy_bindings::DuckdbExtensionCopy,
        extension_types::Duckerror,
    > {
        if self.copy_bindings.is_none() {
            let built = crate::duckdb_extension_copy_bindings::DuckdbExtensionCopy::new(
                self.store.as_context_mut(),
                &self.instance,
            )
            .map_err(map_extension_trap)?;
            self.copy_bindings = Some(built);
        }
        Ok(self.copy_bindings.as_ref().unwrap())
    }

    /// COPY TO: bind a writer for `path`; returns a writer handle.
    pub fn copy_to_bind(
        &mut self,
        handle: u32,
        path: &str,
        columns: &[extension_types::Columndef],
        options: &[(String, String)],
    ) -> Result<u32, extension_types::Duckerror> {
        self.copy_bindings()?;
        let bindings = self.copy_bindings.as_ref().unwrap();
        let guest = bindings.duckdb_extension_copy_dispatch();
        let store = &mut self.store;
        guest
            .call_copy_to_bind(store.as_context_mut(), handle, path, columns, options)
            .map_err(map_extension_trap)?
    }

    /// COPY TO: sink a batch of rows to the writer.
    pub fn copy_to_sink(
        &mut self,
        handle: u32,
        writer: u32,
        rows: &[Vec<extension_types::Duckvalue>],
    ) -> Result<(), extension_types::Duckerror> {
        self.copy_bindings()?;
        let bindings = self.copy_bindings.as_ref().unwrap();
        let guest = bindings.duckdb_extension_copy_dispatch();
        let store = &mut self.store;
        guest
            .call_copy_to_sink(store.as_context_mut(), handle, writer, rows)
            .map_err(map_extension_trap)?
    }

    /// COPY TO: finalize + close; returns rows written.
    pub fn copy_to_finalize(
        &mut self,
        handle: u32,
        writer: u32,
    ) -> Result<u64, extension_types::Duckerror> {
        self.copy_bindings()?;
        let bindings = self.copy_bindings.as_ref().unwrap();
        let guest = bindings.duckdb_extension_copy_dispatch();
        let store = &mut self.store;
        guest
            .call_copy_to_finalize(store.as_context_mut(), handle, writer)
            .map_err(map_extension_trap)?
    }

    /// COPY FROM: bind a reader for `path`; returns (reader handle, columns).
    pub fn copy_from_bind(
        &mut self,
        handle: u32,
        path: &str,
        options: &[(String, String)],
    ) -> Result<CopyFromBindResult, extension_types::Duckerror> {
        self.copy_bindings()?;
        let bindings = self.copy_bindings.as_ref().unwrap();
        let guest = bindings.duckdb_extension_copy_dispatch();
        let store = &mut self.store;
        guest
            .call_copy_from_bind(store.as_context_mut(), handle, path, options)
            .map_err(map_extension_trap)?
    }

    /// COPY FROM: pull up to `max_rows`; empty resultset signals EOF.
    pub fn copy_from_scan(
        &mut self,
        handle: u32,
        reader: u32,
        max_rows: u32,
    ) -> Result<Vec<Vec<extension_types::Duckvalue>>, extension_types::Duckerror> {
        self.copy_bindings()?;
        let bindings = self.copy_bindings.as_ref().unwrap();
        let guest = bindings.duckdb_extension_copy_dispatch();
        let store = &mut self.store;
        guest
            .call_copy_from_scan(store.as_context_mut(), handle, reader, max_rows)
            .map_err(map_extension_trap)?
    }

    /// COPY FROM: close the reader.
    pub fn copy_from_close(
        &mut self,
        handle: u32,
        reader: u32,
    ) -> Result<bool, extension_types::Duckerror> {
        self.copy_bindings()?;
        let bindings = self.copy_bindings.as_ref().unwrap();
        let guest = bindings.duckdb_extension_copy_dispatch();
        let store = &mut self.store;
        guest
            .call_copy_from_close(store.as_context_mut(), handle, reader)
            .map_err(map_extension_trap)?
    }

    // --- 2.1.0 (Item 2): secret-dispatch re-entry ---

    fn secret_bindings(
        &mut self,
    ) -> Result<
        &crate::duckdb_extension_secret_bindings::DuckdbExtensionSecret,
        extension_types::Duckerror,
    > {
        if self.secret_bindings.is_none() {
            let built = crate::duckdb_extension_secret_bindings::DuckdbExtensionSecret::new(
                self.store.as_context_mut(),
                &self.instance,
            )
            .map_err(map_extension_trap)?;
            self.secret_bindings = Some(built);
        }
        Ok(self.secret_bindings.as_ref().unwrap())
    }

    /// Materialize a secret of `(type_name, provider)` from `params`; returns the
    /// resolved flat key=value set the core stores.
    pub fn create_secret(
        &mut self,
        handle: u32,
        type_name: &str,
        provider: &str,
        params: &[SecretKv],
    ) -> Result<Vec<SecretKv>, extension_types::Duckerror> {
        self.secret_bindings()?;
        let bindings = self.secret_bindings.as_ref().unwrap();
        let guest = bindings.duckdb_extension_secret_dispatch();
        let store = &mut self.store;
        guest
            .call_create_secret(store.as_context_mut(), handle, type_name, provider, params)
            .map_err(map_extension_trap)?
    }

    // --- 2.1.0 (Item 4): storage-write-dispatch re-entry ---

    fn storage_write_bindings(
        &mut self,
    ) -> Result<
        &crate::duckdb_extension_storage_write_bindings::DuckdbExtensionStorageWrite,
        extension_types::Duckerror,
    > {
        if self.storage_write_bindings.is_none() {
            let built =
                crate::duckdb_extension_storage_write_bindings::DuckdbExtensionStorageWrite::new(
                    self.store.as_context_mut(),
                    &self.instance,
                )
                .map_err(map_extension_trap)?;
            self.storage_write_bindings = Some(built);
        }
        Ok(self.storage_write_bindings.as_ref().unwrap())
    }

    /// Begin a write transaction on `catalog`; returns a transaction handle.
    pub fn storage_begin_transaction(
        &mut self,
        handle: u32,
        catalog: u32,
    ) -> Result<u32, extension_types::Duckerror> {
        self.storage_write_bindings()?;
        let bindings = self.storage_write_bindings.as_ref().unwrap();
        let guest = bindings.duckdb_extension_storage_write_dispatch();
        let store = &mut self.store;
        guest
            .call_begin_transaction(store.as_context_mut(), handle, catalog)
            .map_err(map_extension_trap)?
    }

    pub fn storage_commit_transaction(
        &mut self,
        handle: u32,
        txn: u32,
    ) -> Result<(), extension_types::Duckerror> {
        self.storage_write_bindings()?;
        let bindings = self.storage_write_bindings.as_ref().unwrap();
        let guest = bindings.duckdb_extension_storage_write_dispatch();
        let store = &mut self.store;
        guest
            .call_commit_transaction(store.as_context_mut(), handle, txn)
            .map_err(map_extension_trap)?
    }

    pub fn storage_rollback_transaction(
        &mut self,
        handle: u32,
        txn: u32,
    ) -> Result<(), extension_types::Duckerror> {
        self.storage_write_bindings()?;
        let bindings = self.storage_write_bindings.as_ref().unwrap();
        let guest = bindings.duckdb_extension_storage_write_dispatch();
        let store = &mut self.store;
        guest
            .call_rollback_transaction(store.as_context_mut(), handle, txn)
            .map_err(map_extension_trap)?
    }

    pub fn storage_create_table(
        &mut self,
        handle: u32,
        txn: u32,
        table: &str,
        columns: &[extension_types::Columndef],
    ) -> Result<(), extension_types::Duckerror> {
        self.storage_write_bindings()?;
        let bindings = self.storage_write_bindings.as_ref().unwrap();
        let guest = bindings.duckdb_extension_storage_write_dispatch();
        let store = &mut self.store;
        guest
            .call_create_table(store.as_context_mut(), handle, txn, table, columns)
            .map_err(map_extension_trap)?
    }

    pub fn storage_insert_rows(
        &mut self,
        handle: u32,
        txn: u32,
        table: &str,
        rows: &[Vec<extension_types::Duckvalue>],
    ) -> Result<u64, extension_types::Duckerror> {
        self.storage_write_bindings()?;
        let bindings = self.storage_write_bindings.as_ref().unwrap();
        let guest = bindings.duckdb_extension_storage_write_dispatch();
        let store = &mut self.store;
        guest
            .call_insert_rows(store.as_context_mut(), handle, txn, table, rows)
            .map_err(map_extension_trap)?
    }

    pub fn storage_delete_rows(
        &mut self,
        handle: u32,
        txn: u32,
        table: &str,
        rowids: &[i64],
    ) -> Result<u64, extension_types::Duckerror> {
        self.storage_write_bindings()?;
        let bindings = self.storage_write_bindings.as_ref().unwrap();
        let guest = bindings.duckdb_extension_storage_write_dispatch();
        let store = &mut self.store;
        guest
            .call_delete_rows(store.as_context_mut(), handle, txn, table, rowids)
            .map_err(map_extension_trap)?
    }

    pub fn storage_update_rows(
        &mut self,
        handle: u32,
        txn: u32,
        table: &str,
        rowids: &[i64],
        rows: &[Vec<extension_types::Duckvalue>],
    ) -> Result<u64, extension_types::Duckerror> {
        self.storage_write_bindings()?;
        let bindings = self.storage_write_bindings.as_ref().unwrap();
        let guest = bindings.duckdb_extension_storage_write_dispatch();
        let store = &mut self.store;
        guest
            .call_update_rows(store.as_context_mut(), handle, txn, table, rowids, rows)
            .map_err(map_extension_trap)?
    }

    // --- 2.2.0 (Item 6): table-stream-dispatch re-entry ---
    // Drives a registered streaming/pushdown table function's exported
    // `table-stream-dispatch`. Types are remapped to base `extension_types` /
    // `extension_runtime` (see lib.rs `with`), so no per-world conversion.

    fn table_stream_bindings(
        &mut self,
    ) -> Result<
        &crate::duckdb_extension_table_stream_bindings::DuckdbExtensionTableStream,
        extension_types::Duckerror,
    > {
        if self.table_stream_bindings.is_none() {
            let built =
                crate::duckdb_extension_table_stream_bindings::DuckdbExtensionTableStream::new(
                    self.store.as_context_mut(),
                    &self.instance,
                )
                .map_err(map_extension_trap)?;
            self.table_stream_bindings = Some(built);
        }
        Ok(self.table_stream_bindings.as_ref().unwrap())
    }

    /// Open a streaming table cursor with bound `args` and a column `projection`
    /// (empty = all columns); returns the cursor handle + projected schema.
    pub fn table_open(
        &mut self,
        handle: u32,
        args: &[extension_types::Duckvalue],
        projection: &[u32],
    ) -> Result<TableOpenResult, extension_types::Duckerror> {
        self.table_stream_bindings()?;
        let bindings = self.table_stream_bindings.as_ref().unwrap();
        let guest = bindings.duckdb_extension_table_stream_dispatch();
        let store = &mut self.store;
        guest
            .call_call_table_open(store.as_context_mut(), handle, args, projection)
            .map_err(map_extension_trap)?
    }

    /// 3.1.0: open a streaming table cursor WITH pushed-down filters (and a column
    /// `projection`, empty = all columns). `filters` is the conjunctive
    /// (AND-of-clauses) neutral filter set the core's streaming `TableFunction`
    /// extracted from the bound plan. A component that ignores the filters stays
    /// correct (the core re-checks them above the scan); honoring them prunes at
    /// the source. Drives the component's `call-table-open-filtered` export.
    pub fn table_open_filtered(
        &mut self,
        handle: u32,
        args: &[extension_types::Duckvalue],
        projection: &[u32],
        filters: &[TableFilter],
    ) -> Result<TableOpenResult, extension_types::Duckerror> {
        self.table_stream_bindings()?;
        let bindings = self.table_stream_bindings.as_ref().unwrap();
        let guest = bindings.duckdb_extension_table_stream_dispatch();
        let store = &mut self.store;
        guest
            .call_call_table_open_filtered(store.as_context_mut(), handle, args, projection, filters)
            .map_err(map_extension_trap)?
    }

    /// Pull up to `max_rows` from the cursor; an empty resultset signals EOF.
    pub fn table_next(
        &mut self,
        handle: u32,
        cursor: u32,
        max_rows: u32,
    ) -> Result<extension_runtime::Resultset, extension_types::Duckerror> {
        self.table_stream_bindings()?;
        let bindings = self.table_stream_bindings.as_ref().unwrap();
        let guest = bindings.duckdb_extension_table_stream_dispatch();
        let store = &mut self.store;
        guest
            .call_call_table_next(store.as_context_mut(), handle, cursor, max_rows)
            .map_err(map_extension_trap)?
    }

    /// Close the streaming cursor and free its state.
    pub fn table_close(
        &mut self,
        handle: u32,
        cursor: u32,
    ) -> Result<bool, extension_types::Duckerror> {
        self.table_stream_bindings()?;
        let bindings = self.table_stream_bindings.as_ref().unwrap();
        let guest = bindings.duckdb_extension_table_stream_dispatch();
        let store = &mut self.store;
        guest
            .call_call_table_close(store.as_context_mut(), handle, cursor)
            .map_err(map_extension_trap)?
    }

    // --- 2.2.0 (Item 6): aggregate-incr-dispatch re-entry ---
    // Drives a registered incremental aggregate's init/update/combine/finalize
    // state machine.

    fn aggregate_incr_bindings(
        &mut self,
    ) -> Result<
        &crate::duckdb_extension_aggregate_incr_bindings::DuckdbExtensionAggregateIncr,
        extension_types::Duckerror,
    > {
        if self.aggregate_incr_bindings.is_none() {
            let built =
                crate::duckdb_extension_aggregate_incr_bindings::DuckdbExtensionAggregateIncr::new(
                    self.store.as_context_mut(),
                    &self.instance,
                )
                .map_err(map_extension_trap)?;
            self.aggregate_incr_bindings = Some(built);
        }
        Ok(self.aggregate_incr_bindings.as_ref().unwrap())
    }

    /// Allocate a fresh incremental-aggregate state; returns a state handle.
    pub fn aggregate_init(&mut self, handle: u32) -> Result<u32, extension_types::Duckerror> {
        self.aggregate_incr_bindings()?;
        let bindings = self.aggregate_incr_bindings.as_ref().unwrap();
        let guest = bindings.duckdb_extension_aggregate_incr_dispatch();
        let store = &mut self.store;
        guest
            .call_call_aggregate_init(store.as_context_mut(), handle)
            .map_err(map_extension_trap)?
    }

    /// Fold a batch of `rows` into the aggregation `state`.
    pub fn aggregate_update(
        &mut self,
        handle: u32,
        state: u32,
        rows: &extension_runtime::Rowbatch,
    ) -> Result<(), extension_types::Duckerror> {
        self.aggregate_incr_bindings()?;
        let bindings = self.aggregate_incr_bindings.as_ref().unwrap();
        let guest = bindings.duckdb_extension_aggregate_incr_dispatch();
        let store = &mut self.store;
        guest
            .call_call_aggregate_update(store.as_context_mut(), handle, state, rows)
            .map_err(map_extension_trap)?
    }

    /// Merge the partial `source` state into `target` (parallel aggregation).
    pub fn aggregate_combine(
        &mut self,
        handle: u32,
        target: u32,
        source: u32,
    ) -> Result<(), extension_types::Duckerror> {
        self.aggregate_incr_bindings()?;
        let bindings = self.aggregate_incr_bindings.as_ref().unwrap();
        let guest = bindings.duckdb_extension_aggregate_incr_dispatch();
        let store = &mut self.store;
        guest
            .call_call_aggregate_combine(store.as_context_mut(), handle, target, source)
            .map_err(map_extension_trap)?
    }

    /// Produce the final value from `state` and free it.
    pub fn aggregate_finalize(
        &mut self,
        handle: u32,
        state: u32,
    ) -> Result<extension_types::Duckvalue, extension_types::Duckerror> {
        self.aggregate_incr_bindings()?;
        let bindings = self.aggregate_incr_bindings.as_ref().unwrap();
        let guest = bindings.duckdb_extension_aggregate_incr_dispatch();
        let store = &mut self.store;
        guest
            .call_call_aggregate_finalize(store.as_context_mut(), handle, state)
            .map_err(map_extension_trap)?
    }

    // --- 2.2.0 (Item 7): conn-dispatch re-entry ---
    // Notifies a component that subscribed via lifecycle.register-connection-callback
    // when a connection is opened or closed.

    fn conn_bindings(
        &mut self,
    ) -> Result<&crate::duckdb_extension_conn_bindings::DuckdbExtensionConn, extension_types::Duckerror>
    {
        if self.conn_bindings.is_none() {
            let built = crate::duckdb_extension_conn_bindings::DuckdbExtensionConn::new(
                self.store.as_context_mut(),
                &self.instance,
            )
            .map_err(map_extension_trap)?;
            self.conn_bindings = Some(built);
        }
        Ok(self.conn_bindings.as_ref().unwrap())
    }

    /// Notify the component that connection `connection_id` was opened.
    pub fn connection_opened(
        &mut self,
        handle: u32,
        connection_id: u64,
    ) -> Result<(), extension_types::Duckerror> {
        self.conn_bindings()?;
        let bindings = self.conn_bindings.as_ref().unwrap();
        let guest = bindings.duckdb_extension_conn_dispatch();
        let store = &mut self.store;
        guest
            .call_on_connection_opened(store.as_context_mut(), handle, connection_id)
            .map_err(map_extension_trap)?
    }

    /// Notify the component that connection `connection_id` was closed.
    pub fn connection_closed(
        &mut self,
        handle: u32,
        connection_id: u64,
    ) -> Result<(), extension_types::Duckerror> {
        self.conn_bindings()?;
        let bindings = self.conn_bindings.as_ref().unwrap();
        let guest = bindings.duckdb_extension_conn_dispatch();
        let store = &mut self.store;
        guest
            .call_on_connection_closed(store.as_context_mut(), handle, connection_id)
            .map_err(map_extension_trap)?
    }

    // --- 2.2.0 (Item 7): file-write-dispatch re-entry ---
    // Drives the writable + glob + stat half of a files backend.

    fn file_write_bindings(
        &mut self,
    ) -> Result<
        &crate::duckdb_extension_file_write_bindings::DuckdbExtensionFileWrite,
        extension_types::Duckerror,
    > {
        if self.file_write_bindings.is_none() {
            let built = crate::duckdb_extension_file_write_bindings::DuckdbExtensionFileWrite::new(
                self.store.as_context_mut(),
                &self.instance,
            )
            .map_err(map_extension_trap)?;
            self.file_write_bindings = Some(built);
        }
        Ok(self.file_write_bindings.as_ref().unwrap())
    }

    /// Write `data` at `offset` in `path`; returns the bytes written.
    pub fn file_write(
        &mut self,
        handle: u32,
        path: &str,
        offset: u64,
        data: &[u8],
    ) -> Result<u64, extension_types::Duckerror> {
        self.file_write_bindings()?;
        let bindings = self.file_write_bindings.as_ref().unwrap();
        let guest = bindings.duckdb_extension_file_write_dispatch();
        let store = &mut self.store;
        guest
            .call_file_write(store.as_context_mut(), handle, path, offset, data)
            .map_err(map_extension_trap)?
    }

    /// Expand a glob `pattern` to matching paths.
    pub fn file_glob(
        &mut self,
        handle: u32,
        pattern: &str,
    ) -> Result<Vec<String>, extension_types::Duckerror> {
        self.file_write_bindings()?;
        let bindings = self.file_write_bindings.as_ref().unwrap();
        let guest = bindings.duckdb_extension_file_write_dispatch();
        let store = &mut self.store;
        guest
            .call_file_glob(store.as_context_mut(), handle, pattern)
            .map_err(map_extension_trap)?
    }

    /// Stat a single `path`.
    pub fn file_stat(
        &mut self,
        handle: u32,
        path: &str,
    ) -> Result<FileInfo, extension_types::Duckerror> {
        self.file_write_bindings()?;
        let bindings = self.file_write_bindings.as_ref().unwrap();
        let guest = bindings.duckdb_extension_file_write_dispatch();
        let store = &mut self.store;
        guest
            .call_file_stat(store.as_context_mut(), handle, path)
            .map_err(map_extension_trap)?
    }

    // --- 2.2.0 (Item 7): index-write-dispatch re-entry ---
    // Drives the general (non-ANN) secondary-index operations: ranged scan,
    // delete, unique-constraint check, and serialization.

    fn index_write_bindings(
        &mut self,
    ) -> Result<
        &crate::duckdb_extension_index_write_bindings::DuckdbExtensionIndexWrite,
        extension_types::Duckerror,
    > {
        if self.index_write_bindings.is_none() {
            let built = crate::duckdb_extension_index_write_bindings::DuckdbExtensionIndexWrite::new(
                self.store.as_context_mut(),
                &self.instance,
            )
            .map_err(map_extension_trap)?;
            self.index_write_bindings = Some(built);
        }
        Ok(self.index_write_bindings.as_ref().unwrap())
    }

    /// Range scan: row-ids whose key is within [low, high] (empty = unbounded).
    pub fn index_scan(
        &mut self,
        handle: u32,
        index: u32,
        low: &[extension_types::Duckvalue],
        high: &[extension_types::Duckvalue],
    ) -> Result<Vec<i64>, extension_types::Duckerror> {
        self.index_write_bindings()?;
        let bindings = self.index_write_bindings.as_ref().unwrap();
        let guest = bindings.duckdb_extension_index_write_dispatch();
        let store = &mut self.store;
        guest
            .call_index_scan(store.as_context_mut(), handle, index, low, high)
            .map_err(map_extension_trap)?
    }

    /// Delete the given `rowids` from the index; returns the number removed.
    pub fn index_delete(
        &mut self,
        handle: u32,
        index: u32,
        rowids: &[i64],
    ) -> Result<u64, extension_types::Duckerror> {
        self.index_write_bindings()?;
        let bindings = self.index_write_bindings.as_ref().unwrap();
        let guest = bindings.duckdb_extension_index_write_dispatch();
        let store = &mut self.store;
        guest
            .call_index_delete(store.as_context_mut(), handle, index, rowids)
            .map_err(map_extension_trap)?
    }

    /// Unique-constraint check: true iff inserting `keys` would violate uniqueness.
    pub fn index_constraint(
        &mut self,
        handle: u32,
        index: u32,
        keys: &[extension_types::Duckvalue],
    ) -> Result<bool, extension_types::Duckerror> {
        self.index_write_bindings()?;
        let bindings = self.index_write_bindings.as_ref().unwrap();
        let guest = bindings.duckdb_extension_index_write_dispatch();
        let store = &mut self.store;
        guest
            .call_index_constraint(store.as_context_mut(), handle, index, keys)
            .map_err(map_extension_trap)?
    }

    /// Serialize the built index to bytes for persistence.
    pub fn index_serialize(
        &mut self,
        handle: u32,
        index: u32,
    ) -> Result<Vec<u8>, extension_types::Duckerror> {
        self.index_write_bindings()?;
        let bindings = self.index_write_bindings.as_ref().unwrap();
        let guest = bindings.duckdb_extension_index_write_dispatch();
        let store = &mut self.store;
        guest
            .call_index_serialize(store.as_context_mut(), handle, index)
            .map_err(map_extension_trap)?
    }

    // --- 2.2.0 (Item 7): settings-dispatch re-entry ---
    // Notifies a component that declared an option (via settings.register-option)
    // and exported settings-dispatch when a user runs `SET <name> = <value>`.

    fn settings_bindings(
        &mut self,
    ) -> Result<
        &crate::duckdb_extension_settings_bindings::DuckdbExtensionSettings,
        extension_types::Duckerror,
    > {
        if self.settings_bindings.is_none() {
            let built = crate::duckdb_extension_settings_bindings::DuckdbExtensionSettings::new(
                self.store.as_context_mut(),
                &self.instance,
            )
            .map_err(map_extension_trap)?;
            self.settings_bindings = Some(built);
        }
        Ok(self.settings_bindings.as_ref().unwrap())
    }

    /// Notify the component that option `name` was SET to `value` (rendered text).
    pub fn setting_set(
        &mut self,
        handle: u32,
        name: &str,
        value: &str,
    ) -> Result<(), extension_types::Duckerror> {
        self.settings_bindings()?;
        let bindings = self.settings_bindings.as_ref().unwrap();
        let guest = bindings.duckdb_extension_settings_dispatch();
        let store = &mut self.store;
        guest
            .call_on_setting_set(store.as_context_mut(), handle, name, value)
            .map_err(map_extension_trap)?
    }

    // --- M2a: storage-dispatch (foreign-catalog) re-entry ---
    // Mirrors the callback-dispatch `dispatch_*` methods but drives the
    // component's exported `storage-dispatch` interface. The native host stages
    // the foreign DB bytes (attach-blob) then attaches, so `storage_attach`
    // reads the host file at `dsn` and hands the bytes to the component.

    /// Stage `bytes` under `dsn`, then open the catalog. Returns the
    /// component-side catalog handle. `handle` is the storage backend's
    /// callback-handle (passed by the component to register-storage).
    pub fn storage_attach(
        &mut self,
        handle: u32,
        dsn: &str,
        bytes: &[u8],
    ) -> Result<u32, extension_types::Duckerror> {
        self.storage_bindings()?;
        // Disjoint field borrows: bindings (immutable) + store (mutable).
        let bindings = self.storage_bindings.as_ref().unwrap();
        let guest = bindings.duckdb_extension_storage_dispatch();
        let store = &mut self.store;
        guest
            .call_attach_blob(store.as_context_mut(), handle, dsn, bytes)
            .map_err(map_extension_trap)?
            .map_err(storage_duckerror_to_ext)?;
        guest
            .call_storage_attach(store.as_context_mut(), handle, dsn, &[])
            .map_err(map_extension_trap)?
            .map_err(storage_duckerror_to_ext)
    }

    pub fn storage_list_tables(
        &mut self,
        handle: u32,
        catalog: u32,
    ) -> Result<Vec<String>, extension_types::Duckerror> {
        self.storage_bindings()?;
        let bindings = self.storage_bindings.as_ref().unwrap();
        let guest = bindings.duckdb_extension_storage_dispatch();
        let store = &mut self.store;
        guest
            .call_storage_list_tables(store.as_context_mut(), handle, catalog)
            .map_err(map_extension_trap)?
            .map_err(storage_duckerror_to_ext)
    }

    pub fn storage_table_columns(
        &mut self,
        handle: u32,
        catalog: u32,
        table: &str,
    ) -> Result<Vec<extension_types::Columndef>, extension_types::Duckerror> {
        self.storage_bindings()?;
        let bindings = self.storage_bindings.as_ref().unwrap();
        let guest = bindings.duckdb_extension_storage_dispatch();
        let store = &mut self.store;
        let cols = guest
            .call_storage_table_columns(store.as_context_mut(), handle, catalog, table)
            .map_err(map_extension_trap)?
            .map_err(storage_duckerror_to_ext)?;
        Ok(cols.into_iter().map(storage_columndef_to_ext).collect())
    }

    /// M2b: open a scan cursor for `(catalog, table)` honoring the request's
    /// projection + filters + limit. Returns the component-side scan handle.
    pub fn storage_scan_open(
        &mut self,
        handle: u32,
        catalog: u32,
        request: storage_scan::ScanRequest,
    ) -> Result<u32, extension_types::Duckerror> {
        self.storage_bindings()?;
        let bindings = self.storage_bindings.as_ref().unwrap();
        let guest = bindings.duckdb_extension_storage_dispatch();
        let store = &mut self.store;
        guest
            .call_storage_scan_open(store.as_context_mut(), handle, catalog, &request)
            .map_err(map_extension_trap)?
            .map_err(storage_duckerror_to_ext)
    }

    /// M2b: pull up to `max_rows` rows from a scan; empty resultset signals EOF.
    pub fn storage_scan_next(
        &mut self,
        handle: u32,
        scan: u32,
        max_rows: u32,
    ) -> Result<Vec<Vec<extension_types::Duckvalue>>, extension_types::Duckerror> {
        self.storage_bindings()?;
        let bindings = self.storage_bindings.as_ref().unwrap();
        let guest = bindings.duckdb_extension_storage_dispatch();
        let store = &mut self.store;
        let rows = guest
            .call_storage_scan_next(store.as_context_mut(), handle, scan, max_rows)
            .map_err(map_extension_trap)?
            .map_err(storage_duckerror_to_ext)?;
        Ok(rows
            .into_iter()
            .map(|row| row.into_iter().map(storage_duckvalue_to_ext).collect())
            .collect())
    }

    /// M2b: close a scan cursor.
    pub fn storage_scan_close(
        &mut self,
        handle: u32,
        scan: u32,
    ) -> Result<bool, extension_types::Duckerror> {
        self.storage_bindings()?;
        let bindings = self.storage_bindings.as_ref().unwrap();
        let guest = bindings.duckdb_extension_storage_dispatch();
        let store = &mut self.store;
        guest
            .call_storage_scan_close(store.as_context_mut(), handle, scan)
            .map_err(map_extension_trap)?
            .map_err(storage_duckerror_to_ext)
    }

    // --- Item 3 / M2a: index-dispatch (custom index build + search) re-entry ---
    // Mirrors the storage-dispatch `storage_*` methods but drives the component's
    // exported `index-dispatch` interface. The HNSW (or other ANN) build happens
    // in-component over a create -> append -> build lifecycle; search returns kNN
    // hits. No callback-handle is threaded (the component keys index state by
    // index NAME), so these take no `handle` argument.

    /// Builds (once) the index-capable bindings from the raw instance. Errors if
    /// this component does not export index-dispatch (i.e. is not an index
    /// backend).
    fn index_bindings(
        &mut self,
    ) -> Result<
        &crate::duckdb_extension_index_bindings::DuckdbExtensionIndex,
        extension_types::Duckerror,
    > {
        if self.index_bindings.is_none() {
            let built = crate::duckdb_extension_index_bindings::DuckdbExtensionIndex::new(
                self.store.as_context_mut(),
                &self.instance,
            )
            .map_err(map_extension_trap)?;
            self.index_bindings = Some(built);
        }
        Ok(self.index_bindings.as_ref().unwrap())
    }

    /// Allocate an empty index builder for `(type_name, index_name)` over a
    /// FLOAT[dims] key. Returns the component-side index-handle.
    pub fn index_create(
        &mut self,
        type_name: &str,
        index_name: &str,
        dims: u32,
    ) -> Result<u32, extension_types::Duckerror> {
        self.index_bindings()?;
        let bindings = self.index_bindings.as_ref().unwrap();
        let guest = bindings.duckdb_extension_index_dispatch();
        let store = &mut self.store;
        guest
            .call_index_create(store.as_context_mut(), type_name, index_name, dims)
            .map_err(map_extension_trap)?
            .map_err(index_duckerror_to_ext)
    }

    /// Accumulate a batch of (rowid, vector) rows into the builder.
    pub fn index_append(
        &mut self,
        handle: u32,
        rowids: &[i64],
        vectors: &[Vec<f32>],
    ) -> Result<(), extension_types::Duckerror> {
        self.index_bindings()?;
        let bindings = self.index_bindings.as_ref().unwrap();
        let guest = bindings.duckdb_extension_index_dispatch();
        let store = &mut self.store;
        guest
            .call_index_append(store.as_context_mut(), handle, rowids, vectors)
            .map_err(map_extension_trap)?
            .map_err(index_duckerror_to_ext)
    }

    /// Finalize: build the ANN map from every appended row.
    pub fn index_build(&mut self, handle: u32) -> Result<(), extension_types::Duckerror> {
        self.index_bindings()?;
        let bindings = self.index_bindings.as_ref().unwrap();
        let guest = bindings.duckdb_extension_index_dispatch();
        let store = &mut self.store;
        guest
            .call_index_build(store.as_context_mut(), handle)
            .map_err(map_extension_trap)?
            .map_err(index_duckerror_to_ext)
    }

    /// k nearest neighbours of `query`, closest first.
    pub fn index_search(
        &mut self,
        handle: u32,
        query: &[f32],
        k: u32,
    ) -> Result<Vec<IndexHit>, extension_types::Duckerror> {
        self.index_bindings()?;
        let bindings = self.index_bindings.as_ref().unwrap();
        let guest = bindings.duckdb_extension_index_dispatch();
        let store = &mut self.store;
        guest
            .call_index_search(store.as_context_mut(), handle, query, k)
            .map_err(map_extension_trap)?
            .map_err(index_duckerror_to_ext)
    }

    /// Free the index + handle.
    pub fn index_drop(&mut self, handle: u32) -> Result<(), extension_types::Duckerror> {
        self.index_bindings()?;
        let bindings = self.index_bindings.as_ref().unwrap();
        let guest = bindings.duckdb_extension_index_dispatch();
        let store = &mut self.store;
        guest
            .call_index_drop(store.as_context_mut(), handle)
            .map_err(map_extension_trap)?
            .map_err(index_duckerror_to_ext)
    }

    // --- httpfs M2: file-dispatch (remote file I/O) re-entry ---
    // Mirrors the storage-dispatch `storage_*` methods but drives the files
    // backend component's exported `file-dispatch` interface. The component
    // fetches the whole resource over wasi:sockets at open, caches it, and
    // serves byte ranges. The error channel is plain strings (not duckerror).

    /// Builds (once) the files-capable bindings from the raw instance. Errors if
    /// this component does not export file-dispatch (i.e. is not a files
    /// backend).
    fn files_bindings(
        &mut self,
    ) -> Result<
        &crate::duckdb_extension_files_bindings::DuckdbExtensionFiles,
        extension_types::Duckerror,
    > {
        if self.files_bindings.is_none() {
            let built = crate::duckdb_extension_files_bindings::DuckdbExtensionFiles::new(
                self.store.as_context_mut(),
                &self.instance,
            )
            .map_err(map_extension_trap)?;
            self.files_bindings = Some(built);
        }
        Ok(self.files_bindings.as_ref().unwrap())
    }

    /// Open (fetch + cache) `url`. Returns (component-side file handle, size).
    /// `handle` is the files backend's callback-handle (from register-files).
    pub fn file_open(
        &mut self,
        handle: u32,
        url: &str,
    ) -> Result<(u32, u64), extension_types::Duckerror> {
        self.files_bindings()?;
        let bindings = self.files_bindings.as_ref().unwrap();
        let guest = bindings.duckdb_extension_file_dispatch();
        let store = &mut self.store;
        let res = guest
            .call_file_open(store.as_context_mut(), handle, url)
            .map_err(map_extension_trap)?
            .map_err(extension_types::Duckerror::Io)?;
        Ok((res.handle, res.size))
    }

    /// Read up to `len` bytes from `file` at `offset`. A short read at EOF is
    /// allowed.
    pub fn file_read(
        &mut self,
        handle: u32,
        file: u32,
        offset: u64,
        len: u32,
    ) -> Result<Vec<u8>, extension_types::Duckerror> {
        self.files_bindings()?;
        let bindings = self.files_bindings.as_ref().unwrap();
        let guest = bindings.duckdb_extension_file_dispatch();
        let store = &mut self.store;
        guest
            .call_file_read(store.as_context_mut(), handle, file, offset, len)
            .map_err(map_extension_trap)?
            .map_err(extension_types::Duckerror::Io)
    }

    /// Drop the component-side cache entry for `file`.
    pub fn file_close(
        &mut self,
        handle: u32,
        file: u32,
    ) -> Result<(), extension_types::Duckerror> {
        self.files_bindings()?;
        let bindings = self.files_bindings.as_ref().unwrap();
        let guest = bindings.duckdb_extension_file_dispatch();
        let store = &mut self.store;
        guest
            .call_file_close(store.as_context_mut(), handle, file)
            .map_err(map_extension_trap)?
            .map_err(extension_types::Duckerror::Io)
    }

    // 2.3.0 / v3: lazily-built parser-dispatch bindings.
    fn parser_bindings(
        &mut self,
    ) -> Result<
        &crate::duckdb_extension_parser_bindings::DuckdbExtensionParser,
        extension_types::Duckerror,
    > {
        if self.parser_bindings.is_none() {
            let built = crate::duckdb_extension_parser_bindings::DuckdbExtensionParser::new(
                self.store.as_context_mut(),
                &self.instance,
            )
            .map_err(map_extension_trap)?;
            self.parser_bindings = Some(built);
        }
        Ok(self.parser_bindings.as_ref().unwrap())
    }

    /// Offer the unrecognized statement `query` to the parser extension `handle`.
    /// Returns `Some(rewrite_sql)` if the component claims it (string->SQL rewrite),
    /// or `None` if it declines. Drives `parser-dispatch.call-parse`.
    pub fn call_parse(
        &mut self,
        handle: u32,
        query: &str,
    ) -> Result<Option<String>, extension_types::Duckerror> {
        self.parser_bindings()?;
        let bindings = self.parser_bindings.as_ref().unwrap();
        let guest = bindings.duckdb_extension_parser_dispatch();
        let store = &mut self.store;
        let outcome = guest
            .call_call_parse(store.as_context_mut(), handle, query)
            .map_err(map_extension_trap)??;
        use crate::duckdb_extension_parser_bindings::exports::duckdb::extension::parser_dispatch::ParseOutcome;
        Ok(match outcome {
            ParseOutcome::Declined => None,
            ParseOutcome::Rewrite(sql) => Some(sql),
        })
    }

    // 2.3.0 / v3: lazily-built optimizer-dispatch bindings.
    fn optimizer_bindings(
        &mut self,
    ) -> Result<
        &crate::duckdb_extension_optimizer_bindings::DuckdbExtensionOptimizer,
        extension_types::Duckerror,
    > {
        if self.optimizer_bindings.is_none() {
            let built = crate::duckdb_extension_optimizer_bindings::DuckdbExtensionOptimizer::new(
                self.store.as_context_mut(),
                &self.instance,
            )
            .map_err(map_extension_trap)?;
            self.optimizer_bindings = Some(built);
        }
        Ok(self.optimizer_bindings.as_ref().unwrap())
    }

    /// Offer the flattened plan (`nodes` = (id, op-type, parent, params-json);
    /// `query` = the source SQL or empty) to the optimizer rule `handle`. Returns
    /// `Some(rewrite_sql)` for a `rewrite-query` directive, or `None` for declined /
    /// a structured `apply` directive (not driven via SQL re-plan). Drives
    /// `optimizer-dispatch.call-optimize`.
    pub fn call_optimize(
        &mut self,
        handle: u32,
        nodes: Vec<(u32, String, Option<u32>, String)>,
        query: &str,
    ) -> Result<Option<String>, extension_types::Duckerror> {
        use crate::duckdb_extension_optimizer_bindings::exports::duckdb::extension::optimizer_dispatch::{
            PlanNode, PlanShape, RewriteDirective,
        };
        self.optimizer_bindings()?;
        let bindings = self.optimizer_bindings.as_ref().unwrap();
        let guest = bindings.duckdb_extension_optimizer_dispatch();
        let plan_nodes: Vec<PlanNode> = nodes
            .into_iter()
            .map(|(id, op_type, parent, params_json)| PlanNode {
                id,
                op_type,
                parent,
                params_json,
            })
            .collect();
        let plan = PlanShape {
            nodes: plan_nodes,
            query: query.to_string(),
        };
        let store = &mut self.store;
        let directive = guest
            .call_call_optimize(store.as_context_mut(), handle, &plan)
            .map_err(map_extension_trap)??;
        Ok(match directive {
            RewriteDirective::Declined => None,
            RewriteDirective::RewriteQuery(sql) => Some(sql),
            // Structured rewrites are not (yet) applied via SQL re-plan; treat as
            // declined so the core keeps the original plan.
            RewriteDirective::Apply(_) => None,
        })
    }
}

// ---------------------------------------------------------------------------
// Tests: the pure capture conversions + the capture-into-pending logic.
//
// These exercise the trust-boundary converters (a component-supplied WIT value
// turned into a neutral `reg::*`) and the storage/index world -> base-world
// converters WITHOUT needing wasmtime to instantiate a component. The Host
// trait impls that capture registrations DO need an `ExtensionStoreState`, which
// we build with a no-op services sink and an empty wasi context.
// ---------------------------------------------------------------------------
#[cfg(test)]
mod tests {
    use super::*;

    /// A no-op `ExtensionServices` sink: every config read is unavailable, logs
    /// are dropped. Lets us build an `ExtensionStoreState` to test the capture
    /// paths without a live database.
    struct NoopServices;
    impl ExtensionServices for NoopServices {
        fn provider_version(&mut self) -> Result<String, ConfigError> {
            Ok("test".to_string())
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
        fn log(&mut self, _level: LogLevel, _message: &str, _target: Option<&str>) {}
        fn log_fields(&mut self, _level: LogLevel, _message: &str, _fields: &[LogField]) {}
    }

    fn test_state() -> ExtensionStoreState {
        let wasi = wasmtime_wasi::WasiCtxBuilder::new().build();
        ExtensionStoreState::new(
            wasi,
            Box::new(NoopServices),
            Arc::new(Mutex::new(CallbackRegistry::default())),
            "testext".to_string(),
        )
    }

    /// Every base-world logicaltype, including the rich set, for round-tripping.
    fn all_ext_logicaltypes() -> Vec<extension_runtime::Logicaltype> {
        use extension_runtime::Logicaltype as L;
        vec![
            L::Boolean,
            L::Int64,
            L::Uint64,
            L::Float64,
            L::Text,
            L::Blob,
            L::Int32,
            L::Timestamp,
            L::Int8,
            L::Int16,
            L::Uint8,
            L::Uint16,
            L::Uint32,
            L::Float32,
            L::Date,
            L::Time,
            L::Timestamptz,
            L::Decimal,
            L::Interval,
            L::Uuid,
            L::Complex("STRUCT(a INTEGER, b VARCHAR)".to_string()),
        ]
    }

    /// Build a component-model engine (with wasm-exceptions) the way the host does.
    fn test_engine() -> Engine {
        let mut config = wasmtime::Config::new();
        config.wasm_component_model(true);
        config.wasm_exceptions(true);
        Engine::new(&config).expect("engine")
    }

    fn load_artifact(engine: &Engine, name: &str) -> wasmtime::Result<ExtensionInstance> {
        let manifest = env!("CARGO_MANIFEST_DIR");
        let path = std::path::Path::new(manifest)
            .join("../../artifacts/extensions")
            .join(format!("{name}.wasm"));
        let bytes = std::fs::read(&path).expect("read artifact");
        let component = Component::new(engine, &bytes)?;
        let wasi = wasmtime_wasi::WasiCtxBuilder::new().inherit_stderr().build();
        load_component(
            engine,
            &component,
            wasi,
            Box::new(NoopServices),
            Arc::new(Mutex::new(CallbackRegistry::default())),
            name.to_string(),
        )
    }

    /// THE ADDITIVE-MINOR PROOF (3.1.0). The freeze policy's whole point: an
    /// existing component built at the FROZEN @3.0.0 baseline loads UN-REBUILT on
    /// the @3.1.0 host. The host now provides the new additive `table-stream`
    /// import (CONTRACT_MINOR = 1); a @3.0.0 component imports a strict SUBSET of
    /// what the host provides, so the wasmtime component linker semver-matches it
    /// and `load()` runs. Only a component that OPTS INTO filter pushdown rebuilds.
    ///
    /// Loads the shipped @3.0.0 `aba` + `geohash` artifacts (scalar components that
    /// do NOT import `table-stream`) against the 3.1.0 host. Skipped gracefully if
    /// the artifacts are absent (the wasm toolchain-free CI subset).
    #[test]
    fn additive_minor_loads_frozen_3_0_0_components_unrebuilt() {
        let manifest = env!("CARGO_MANIFEST_DIR");
        let aba = std::path::Path::new(manifest).join("../../artifacts/extensions/aba.wasm");
        if !aba.exists() {
            eprintln!("skipping additive-load proof: artifacts/extensions/aba.wasm absent");
            return;
        }

        // Sanity: this host is @3.MINOR with MINOR >= 1 (the additive bump landed).
        assert_eq!(crate::CONTRACT_MAJOR, 3);
        assert!(
            crate::CONTRACT_MINOR >= 1,
            "the additive minor must have bumped CONTRACT_MINOR to >= 1"
        );

        let engine = test_engine();
        for name in ["aba", "geohash"] {
            let path = std::path::Path::new(manifest)
                .join("../../artifacts/extensions")
                .join(format!("{name}.wasm"));
            if !path.exists() {
                continue;
            }
            let bytes = std::fs::read(&path).unwrap();
            let component = Component::new(&engine, &bytes).unwrap();

            // The artifact is a FROZEN @3.0.0 component (minor 0).
            let ver = crate::component_contract_version(&engine, &component);
            assert_eq!(
                ver,
                Some((3, 0)),
                "{name} must be the frozen @3.0.0 baseline (un-rebuilt)"
            );

            // The contract guard ADMITS it on the 3.1.0 host (minor forward-compat).
            crate::check_component_contract(&engine, &component, name)
                .unwrap_or_else(|e| panic!("{name} rejected by the 3.1.0 contract guard: {e}"));

            // And it INSTANTIATES + runs load() un-rebuilt against the 3.1.0 host
            // linker (which now also provides the new `table-stream` import).
            let inst = load_artifact(&engine, name)
                .unwrap_or_else(|e| panic!("{name} failed to load on the 3.1.0 host: {e}"));
            drop(inst);
            eprintln!("[additive-load] @3.0.0 '{name}' loaded UN-REBUILT on the 3.1.0 host");
        }
    }

    #[test]
    fn convert_logicaltype_covers_every_arm_incl_rich_and_complex() {
        use extension_runtime::Logicaltype as L;
        assert_eq!(
            convert_extension_logicaltype(L::Boolean),
            reg::LogicalType::Boolean
        );
        assert_eq!(convert_extension_logicaltype(L::Int8), reg::LogicalType::Int8);
        assert_eq!(
            convert_extension_logicaltype(L::Uint32),
            reg::LogicalType::Uint32
        );
        assert_eq!(
            convert_extension_logicaltype(L::Timestamptz),
            reg::LogicalType::Timestamptz
        );
        assert_eq!(convert_extension_logicaltype(L::Uuid), reg::LogicalType::Uuid);
        // The escape-hatch Complex arm carries its owned type-expr through.
        let cx = convert_extension_logicaltype(L::Complex("INTEGER[]".to_string()));
        assert_eq!(cx, reg::LogicalType::Complex("INTEGER[]".to_string()));
        assert_eq!(cx.describe(), "INTEGER[]");
        // Every arm converts without panicking and yields a non-empty label.
        for ty in all_ext_logicaltypes() {
            assert!(!convert_extension_logicaltype(ty).describe().is_empty());
        }
    }

    #[test]
    fn convert_funcargs_preserves_names_and_types() {
        use extension_runtime::Logicaltype as L;
        let args = vec![
            extension_runtime::Funcarg {
                name: Some("x".to_string()),
                logical: L::Int64,
            },
            extension_runtime::Funcarg {
                name: None,
                logical: L::Text,
            },
        ];
        let out = convert_extension_funcargs(args);
        assert_eq!(out.len(), 2);
        assert_eq!(out[0].name.as_deref(), Some("x"));
        assert_eq!(out[0].logical, reg::LogicalType::Int64);
        assert_eq!(out[1].name, None);
        assert_eq!(out[1].logical, reg::LogicalType::Text);
    }

    #[test]
    fn convert_columndefs_preserves_names_and_types() {
        use extension_runtime::Logicaltype as L;
        let cols = vec![
            extension_runtime::Columndef {
                name: "id".to_string(),
                logical: L::Int32,
            },
            extension_runtime::Columndef {
                name: "label".to_string(),
                logical: L::Complex("VARCHAR[]".to_string()),
            },
        ];
        let out = convert_extension_columndefs(cols);
        assert_eq!(out.len(), 2);
        assert_eq!(out[0].name, "id");
        assert_eq!(out[0].logical, reg::LogicalType::Int32);
        assert_eq!(
            out[1].logical,
            reg::LogicalType::Complex("VARCHAR[]".to_string())
        );
    }

    #[test]
    fn convert_funcflags_maps_each_bit() {
        let none = convert_extension_funcflags(extension_types::Funcflags::empty());
        assert_eq!(none, reg::FuncFlags::default());
        let all = convert_extension_funcflags(
            extension_types::Funcflags::DETERMINISTIC
                | extension_types::Funcflags::COMMUTATIVE
                | extension_types::Funcflags::STATELESS
                | extension_types::Funcflags::SIDEEFFECTING
                | extension_types::Funcflags::DEPRECATED,
        );
        assert!(all.deterministic && all.commutative && all.stateless);
        assert!(all.side_effecting && all.deprecated);
        let det = convert_extension_funcflags(extension_types::Funcflags::DETERMINISTIC);
        assert!(det.deterministic);
        assert!(!det.commutative && !det.stateless && !det.side_effecting && !det.deprecated);
    }

    #[test]
    fn storage_duckvalue_converts_every_arm_incl_rich() {
        use storage_types::Duckvalue as S;
        let samples = vec![
            S::Null,
            S::Boolean(true),
            S::Int64(-9),
            S::Uint64(9),
            S::Float64(1.5),
            S::Text("hi".to_string()),
            S::Blob(vec![1, 2, 3]),
            S::Int32(-3),
            S::Timestamp(100),
            S::Int8(-1),
            S::Int16(-2),
            S::Uint8(1),
            S::Uint16(2),
            S::Uint32(3),
            S::Float32(0.25),
            S::Date(42),
            S::Time(7),
            S::Timestamptz(8),
            S::Decimal(storage_types::Decimalvalue {
                lower: 123,
                upper: 0,
                width: 5,
                scale: 2,
            }),
            S::Interval(storage_types::Intervalvalue {
                months: 1,
                days: 2,
                micros: 3,
            }),
            S::Uuid(storage_types::Uuidvalue { hi: 1, lo: 2 }),
            S::Complex(storage_types::Complexvalue {
                type_expr: "INTEGER[]".to_string(),
                json: "[1,2]".to_string(),
            }),
        ];
        for s in samples {
            let ext = storage_duckvalue_to_ext(s);
            match ext {
                extension_types::Duckvalue::Decimal(ref d) => {
                    assert_eq!((d.lower, d.width, d.scale), (123, 5, 2));
                }
                extension_types::Duckvalue::Complex(ref c) => {
                    assert_eq!(c.type_expr, "INTEGER[]");
                }
                _ => {}
            }
        }
    }

    #[test]
    fn storage_logicaltype_and_columndef_convert_every_arm() {
        use storage_types::Logicaltype as S;
        for ty in [
            S::Boolean,
            S::Int64,
            S::Uint64,
            S::Float64,
            S::Text,
            S::Blob,
            S::Int32,
            S::Timestamp,
            S::Int8,
            S::Int16,
            S::Uint8,
            S::Uint16,
            S::Uint32,
            S::Float32,
            S::Date,
            S::Time,
            S::Timestamptz,
            S::Decimal,
            S::Interval,
            S::Uuid,
        ] {
            let _ = storage_logicaltype_to_ext(ty);
        }
        let cx = storage_logicaltype_to_ext(S::Complex("STRUCT(a INT)".to_string()));
        assert!(matches!(cx, extension_types::Logicaltype::Complex(ref e) if e == "STRUCT(a INT)"));
        let col = storage_columndef_to_ext(storage_types::Columndef {
            name: "c".to_string(),
            logical: S::Int64,
        });
        assert_eq!(col.name, "c");
    }

    #[test]
    fn storage_and_index_duckerror_map_every_arm() {
        for e in [
            storage_types::Duckerror::Invalidargument("a".into()),
            storage_types::Duckerror::Unsupported("b".into()),
            storage_types::Duckerror::Invalidstate("c".into()),
            storage_types::Duckerror::Io("d".into()),
            storage_types::Duckerror::Internal("e".into()),
        ] {
            let _ = storage_duckerror_to_ext(e);
        }
        for e in [
            index_types::Duckerror::Invalidargument("a".into()),
            index_types::Duckerror::Unsupported("b".into()),
            index_types::Duckerror::Invalidstate("c".into()),
            index_types::Duckerror::Io("d".into()),
            index_types::Duckerror::Internal("e".into()),
        ] {
            let _ = index_duckerror_to_ext(e);
        }
    }

    #[test]
    fn configerror_and_loglevel_converters_cover_arms() {
        for e in [
            ConfigError::InvalidKey("k".into()),
            ConfigError::TypeMismatch("t".into()),
            ConfigError::Unavailable("u".into()),
            ConfigError::InternalConfig("i".into()),
        ] {
            let _ = neutral_configerror_to_ext(e);
        }
        for l in [
            extension_logging::Loglevel::Trace,
            extension_logging::Loglevel::Debug,
            extension_logging::Loglevel::Info,
            extension_logging::Loglevel::Warn,
            extension_logging::Loglevel::Error,
        ] {
            let _ = ext_loglevel_to_neutral(l);
        }
    }

    // --- capture-into-pending logic (Host trait impls) ---

    #[test]
    fn register_collation_captures_into_pending_and_is_drained() {
        let mut state = test_state();
        // A malformed/empty name must still be captured (never panic); the core
        // is responsible for rejecting it later.
        extension_collation::Host::register_collation(
            &mut state,
            String::new(),
            "transform".to_string(),
            true,
        )
        .expect("register_collation should not error");
        extension_collation::Host::register_collation(
            &mut state,
            "icu_en".to_string(),
            "icu_sort".to_string(),
            false,
        )
        .expect("register_collation should not error");
        let drained = state.take_pending_collations();
        assert_eq!(drained.len(), 2);
        assert_eq!(drained[0].name, "");
        assert_eq!(drained[1].name, "icu_en");
        assert_eq!(drained[1].transform_scalar, "icu_sort");
        // Draining again yields nothing (mem::take semantics).
        assert!(state.take_pending_collations().is_empty());
    }

    #[test]
    fn register_index_type_captures_into_pending() {
        let mut state = test_state();
        extension_index::Host::register_index_type(&mut state, "wasm_hnsw".to_string())
            .expect("register_index_type should not error");
        let drained = state.take_pending_indexes();
        assert_eq!(drained.len(), 1);
        assert_eq!(drained[0].type_name, "wasm_hnsw");
        assert_eq!(drained[0].extension, "testext");
    }

    #[test]
    fn register_storage_and_files_capture_into_pending() {
        let mut state = test_state();
        extension_storage::Host::register_storage(&mut state, "sqlitewasm".to_string(), 7, None)
            .expect("register_storage should not error");
        let storages = state.take_pending_storages();
        assert_eq!(storages.len(), 1);
        assert_eq!(storages[0].type_name, "sqlitewasm");
        assert_eq!(storages[0].callback_handle, 7);

        extension_files_reg::Host::register_files(&mut state, 9)
            .expect("register_files should not error");
        let files = state.take_pending_files();
        assert_eq!(files.len(), 1);
        assert_eq!(files[0].callback_handle, 9);
    }

    #[test]
    fn register_logical_type_and_macro_capture_into_pending() {
        let mut state = test_state();
        extension_catalog::Host::register_logical_type(
            &mut state,
            extension_catalog::LogicalType {
                name: "myint".to_string(),
                physical: "INTEGER".to_string(),
            },
        )
        .expect("register_logical_type should not error");
        extension_catalog::Host::register_macro(
            &mut state,
            extension_catalog::MacroDef {
                schema: "main".to_string(),
                name: "addone".to_string(),
                parameters: vec!["x".to_string()].into(),
                definition_sql: "x + 1".to_string(),
            },
        )
        .expect("register_macro should not error");
        let drained = state.drain_pending();
        assert_eq!(drained.logical_types.len(), 1);
        assert_eq!(drained.logical_types[0].name, "myint");
        assert_eq!(drained.macros.len(), 1);
        assert_eq!(drained.macros[0].name, "addone");
        assert_eq!(drained.macros[0].parameters, vec!["x".to_string()]);
    }

    #[test]
    fn register_copy_handler_captures_into_pending() {
        // 2.1.0 (Item 1): copy handlers are now CAPTURED (driven through
        // copy-dispatch), not rejected. Registration succeeds and lands in the
        // neutral pending buffer with the routing function-handle preserved.
        let mut state = test_state();
        let res = extension_files::Host::register_copy_handler(
            &mut state,
            extension_files::CopyHandler {
                extension: "parquet".to_string(),
                function: 7,
            },
        );
        assert!(res.is_ok());
        let captured = state.take_pending_copy_handlers();
        assert_eq!(captured.len(), 1);
        assert_eq!(captured[0].file_extension, "parquet");
        assert_eq!(captured[0].function_handle, 7);
    }

    #[test]
    fn registers_2_1_0_additive_capabilities_into_pending() {
        // 2.1.0: secret type + provider, settings option, table macro, modified
        // logical type, and enum all CAPTURE into their neutral pending buffers.
        let mut state = test_state();

        extension_secret::Host::register_secret_type(
            &mut state,
            "s3".to_string(),
            vec![
                extension_secret::SecretParam { name: "key_id".to_string(), redacted: false },
                extension_secret::SecretParam { name: "secret".to_string(), redacted: true },
            ]
            .into(),
            11,
        )
        .expect("register_secret_type");
        extension_secret::Host::register_secret_provider(
            &mut state,
            "s3".to_string(),
            "credential_chain".to_string(),
            12,
        )
        .expect("register_secret_provider");

        extension_settings::Host::register_option(
            &mut state,
            "my_threshold".to_string(),
            "tuning knob".to_string(),
            extension_settings::SettingType::Bigint,
            Some("42".to_string()),
            extension_settings::SettingScope::Global,
        )
        .expect("register_option");

        extension_macro_ext::Host::register_table_macro(
            &mut state,
            "main".to_string(),
            "series".to_string(),
            vec!["n".to_string()].into(),
            "SELECT * FROM range(n)".to_string(),
        )
        .expect("register_table_macro");

        extension_types_ext::Host::register_logical_type_modified(
            &mut state,
            "price".to_string(),
            "DECIMAL(18,3)".to_string(),
        )
        .expect("register_logical_type_modified");
        extension_types_ext::Host::register_enum(
            &mut state,
            "mood".to_string(),
            vec!["happy".to_string(), "sad".to_string()].into(),
        )
        .expect("register_enum");

        let secrets = state.take_pending_secrets();
        assert_eq!(secrets.len(), 2);
        assert_eq!(secrets[0].type_name, "s3");
        assert_eq!(secrets[0].params.len(), 2);
        assert!(secrets[0].params[1].1, "secret param must be redacted");
        assert_eq!(secrets[0].callback_handle, 11);
        assert_eq!(secrets[1].provider.as_deref(), Some("credential_chain"));

        let settings = state.take_pending_settings();
        assert_eq!(settings.len(), 1);
        assert_eq!(settings[0].name, "my_threshold");
        assert_eq!(settings[0].ty, "bigint");
        assert_eq!(settings[0].scope, "global");
        assert_eq!(settings[0].default_value.as_deref(), Some("42"));

        let macros = state.take_pending_table_macros();
        assert_eq!(macros.len(), 1);
        assert_eq!(macros[0].name, "series");

        let modified = state.take_pending_modified_types();
        assert_eq!(modified.len(), 1);
        assert_eq!(modified[0].type_expr, "DECIMAL(18,3)");

        let enums = state.take_pending_enum_types();
        assert_eq!(enums.len(), 1);
        assert_eq!(enums[0].members, vec!["happy".to_string(), "sad".to_string()]);
    }

    #[test]
    fn registers_2_2_0_additive_capabilities_into_pending() {
        // 2.2.0 (Items 6-7): the richer scalar (scalar-ex), connection-lifecycle
        // subscription, coordinate system, Arrow table, text encoding, and
        // compression codec all CAPTURE into their neutral pending buffers.
        use extension_runtime::Logicaltype as L;
        let mut state = test_state();

        // Item 6: register-scalar-ex with varargs + special NULL handling.
        extension_runtime_ext::Host::register_scalar_ex(
            &mut state,
            "concat_ws".to_string(),
            vec![extension_runtime_ext::Funcarg { name: Some("sep".to_string()), logical: L::Text }]
                .into(),
            Some(L::Text),
            L::Text,
            extension_runtime_ext::NullHandling::Special,
            21,
            None,
        )
        .expect("register_scalar_ex");

        // Item 7: connection-lifecycle subscription (opened only).
        extension_lifecycle::Host::register_connection_callback(
            &mut state,
            extension_lifecycle::ConnEvents::OPENED,
            22,
        )
        .expect("register_connection_callback");

        // Item 7: coordinate system.
        extension_coordinate_system::Host::register_coordinate_system(
            &mut state,
            extension_coordinate_system::CrsDef {
                auth_name: "EPSG".to_string(),
                code: 4326,
                wkt: "GEOGCRS[...]".to_string(),
            },
        )
        .expect("register_coordinate_system");

        // Item 7: Arrow table producer.
        extension_arrow_ext::Host::register_arrow_table(
            &mut state,
            "feed".to_string(),
            vec![extension_arrow_ext::Columndef { name: "v".to_string(), logical: L::Int64 }].into(),
            23,
        )
        .expect("register_arrow_table");

        // Item 7: text encoding.
        extension_encoding::Host::register_encoding(
            &mut state,
            "latin-1".to_string(),
            vec!["iso-8859-1".to_string()].into(),
            24,
        )
        .expect("register_encoding");

        // Item 7: compression codec.
        extension_compression::Host::register_compression(
            &mut state,
            "zstd".to_string(),
            "zst".to_string(),
            25,
        )
        .expect("register_compression");

        let scalar_ex = state.take_pending_scalar_ex();
        assert_eq!(scalar_ex.len(), 1);
        assert_eq!(scalar_ex[0].name, "concat_ws");
        assert_eq!(scalar_ex[0].extension, "testext");
        assert!(scalar_ex[0].special_null, "special NULL handling must be captured");
        assert_eq!(scalar_ex[0].varargs, Some(reg::LogicalType::Text));
        assert_eq!(scalar_ex[0].callback_handle, 21);

        let conn = state.take_pending_conn_callbacks();
        assert_eq!(conn.len(), 1);
        assert!(conn[0].on_opened);
        assert!(!conn[0].on_closed);
        assert_eq!(conn[0].callback_handle, 22);

        let crs = state.take_pending_coordinate_systems();
        assert_eq!(crs.len(), 1);
        assert_eq!(crs[0].auth_name, "EPSG");
        assert_eq!(crs[0].code, 4326);

        let arrow = state.take_pending_arrow_tables();
        assert_eq!(arrow.len(), 1);
        assert_eq!(arrow[0].name, "feed");
        assert_eq!(arrow[0].columns.len(), 1);
        assert_eq!(arrow[0].callback_handle, 23);

        let encodings = state.take_pending_encodings();
        assert_eq!(encodings.len(), 1);
        assert_eq!(encodings[0].name, "latin-1");
        assert_eq!(encodings[0].aliases, vec!["iso-8859-1".to_string()]);

        let compressions = state.take_pending_compressions();
        assert_eq!(compressions.len(), 1);
        assert_eq!(compressions[0].name, "zstd");
        assert_eq!(compressions[0].file_extension, "zst");
        assert_eq!(compressions[0].callback_handle, 25);
    }

    #[test]
    fn nested_value_rides_complex_arm_without_new_type_arm() {
        // Nested LIST/STRUCT values ride the EXISTING `complex(type-expr, json)`
        // escape hatch on `duckvalue` -- no new `duckvalue`/`logicaltype` arm, so
        // the bump stays additive (2.1.0). This asserts a flat-encoded LIST value
        // is carried through the base types verbatim (the CORE reconstructs the
        // real LIST vector from the type-expr + JSON via the duckdb C vector API).
        let v = extension_types::Duckvalue::Complex(extension_types::Complexvalue {
            type_expr: "INTEGER[]".to_string(),
            json: "[10,20,30]".to_string(),
        });
        match v {
            extension_types::Duckvalue::Complex(c) => {
                assert_eq!(c.type_expr, "INTEGER[]");
                assert_eq!(c.json, "[10,20,30]");
            }
            _ => panic!("expected complex arm"),
        }
    }

    #[test]
    fn replacement_scan_unknown_table_handle_errors_not_panics() {
        let mut state = test_state();
        // No table function was ever registered, so handle 999 is unknown: the
        // capture must return Err, not panic.
        let res = extension_files::Host::register_replacement_scan(
            &mut state,
            extension_files::ReplacementScan {
                table_function: 999,
                extensions: vec!["csv".to_string()].into(),
                mode: extension_files::DetectionMode::ExtensionOnly,
            },
        );
        assert!(res.is_err());
    }

    #[test]
    fn register_pragma_with_unknown_callback_handle_errors_not_panics() {
        let mut state = test_state();
        // A pragma callback handle that was never registered in the callback
        // registry -> Err, not a panic.
        let bogus: Resource<extension_runtime::PragmaCallback> = Resource::new_own(424242);
        let registry: Resource<extension_runtime::PragmaRegistry> = Resource::new_own(1);
        let res = extension_runtime::HostPragmaRegistry::register_call(
            &mut state,
            registry,
            "my_pragma".to_string(),
            Vec::new().into(),
            extension_runtime::Logicaltype::Text,
            bogus,
            None,
        );
        assert!(res.is_err());
    }

    #[test]
    fn drain_pending_is_empty_on_fresh_state() {
        let mut state = test_state();
        let drained = state.drain_pending();
        assert!(drained.scalars.is_empty());
        assert!(drained.tables.is_empty());
        assert!(drained.aggregates.is_empty());
        assert!(drained.macros.is_empty());
        assert!(drained.logical_types.is_empty());
    }

    #[test]
    fn summarize_registration_names_truncates_with_more() {
        let names = ["a", "b", "c", "d", "e"];
        let s = summarize_registration_names(&names, |n| n);
        assert!(s.contains('a'));
        assert!(s.contains("+2 more"));
        assert_eq!(summarize_registration_names::<&str, _>(&[], |n| n), "none");
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
    extension_index::add_to_linker::<ExtensionStoreState, ExtensionStoreState>(linker, |s| s)?;
    extension_collation::add_to_linker::<ExtensionStoreState, ExtensionStoreState>(linker, |s| s)?;
    extension_files_reg::add_to_linker::<ExtensionStoreState, ExtensionStoreState>(linker, |s| s)?;
    extension_query::add_to_linker::<ExtensionStoreState, ExtensionStoreState>(linker, |s| s)?;
    // 2.1.0 additive registration imports.
    extension_secret::add_to_linker::<ExtensionStoreState, ExtensionStoreState>(linker, |s| s)?;
    extension_settings::add_to_linker::<ExtensionStoreState, ExtensionStoreState>(linker, |s| s)?;
    extension_macro_ext::add_to_linker::<ExtensionStoreState, ExtensionStoreState>(linker, |s| s)?;
    extension_types_ext::add_to_linker::<ExtensionStoreState, ExtensionStoreState>(linker, |s| s)?;
    // 2.2.0 additive registration imports (Items 6-7).
    extension_runtime_ext::add_to_linker::<ExtensionStoreState, ExtensionStoreState>(linker, |s| s)?;
    extension_lifecycle::add_to_linker::<ExtensionStoreState, ExtensionStoreState>(linker, |s| s)?;
    extension_coordinate_system::add_to_linker::<ExtensionStoreState, ExtensionStoreState>(
        linker,
        |s| s,
    )?;
    extension_arrow_ext::add_to_linker::<ExtensionStoreState, ExtensionStoreState>(linker, |s| s)?;
    extension_encoding::add_to_linker::<ExtensionStoreState, ExtensionStoreState>(linker, |s| s)?;
    extension_compression::add_to_linker::<ExtensionStoreState, ExtensionStoreState>(linker, |s| s)?;
    // 2.3.0 / v3 additive registration imports.
    extension_parser::add_to_linker::<ExtensionStoreState, ExtensionStoreState>(linker, |s| s)?;
    extension_optimizer::add_to_linker::<ExtensionStoreState, ExtensionStoreState>(linker, |s| s)?;
    // 3.1.0 additive registration import: filterable streaming table-fn marker.
    extension_table_stream::add_to_linker::<ExtensionStoreState, ExtensionStoreState>(linker, |s| s)?;
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
    load_component_with_dynlink(
        engine,
        component,
        wasi,
        services,
        callback_registry,
        extension_name,
        None,
    )
}

/// Like [`load_component`] but also wires `compose:dynlink/linker` for a
/// component that imports it: the host import is added to the guest linker
/// (gated on `imports_linker`) and a [`DynLinkBridge`](crate::compose_dynlink::DynLinkBridge)
/// over the supplied shared provider `registry` is moved into the store
/// state. This is how an `ml_kmeans`-style aggregate reaches the one resident,
/// shared pylon provider. A component that does NOT import the linker (every
/// other extension) is unaffected even if a registry is supplied.
pub fn load_component_with_dynlink(
    engine: &Engine,
    component: &Component,
    wasi: WasiCtx,
    services: Box<dyn ExtensionServices>,
    callback_registry: Arc<Mutex<CallbackRegistry>>,
    extension_name: String,
    dynlink_registry: Option<crate::compose_dynlink::ProviderRegistry>,
) -> wasmtime::Result<ExtensionInstance> {
    // Contract guard: reject a component whose duckdb:extension contract major
    // differs from this host's (or is unversioned/legacy) BEFORE instantiating,
    // so a mismatched component never silently marshals corrupted values.
    crate::check_component_contract(engine, component, &extension_name)?;

    let mut linker = Linker::<ExtensionStoreState>::new(engine);
    add_extension_interfaces_to_linker(&mut linker)?;

    // compose:dynlink/linker: conditionally satisfy a guest-driven provider
    // import. ONLY a component that actually imports the linker gets the host
    // import + a bridge; every other extension pays nothing (the gate mirrors
    // the framework's `imports_linker`).
    let dynlink = match dynlink_registry {
        Some(registry) if crate::compose_dynlink::imports_linker(engine, component) => {
            eprintln!(
                "[extension-runtime:{extension_name}] imports compose:dynlink/linker; wiring the shared-provider bridge"
            );
            crate::compose_dynlink::add_to_linker::<ExtensionStoreState>(&mut linker)
                .map_err(|e| wasmtime::Error::msg(e.to_string()))?;
            Some(crate::compose_dynlink::new_resident(registry))
        }
        _ => None,
    };

    let mut store = Store::new(
        engine,
        ExtensionStoreState::with_dynlink(
            wasi,
            services,
            callback_registry,
            extension_name.clone(),
            dynlink,
        ),
    );

    // Instantiate via the linker to obtain the raw component instance, then build
    // the typed base-world bindings from it. Retaining the raw instance lets a
    // storage backend lazily build the storage-capable bindings later (the base
    // world doesn't mandate storage-dispatch, so non-storage extensions still
    // load here).
    let instance_pre = linker.instantiate_pre(component)?;
    let instance = instance_pre.instantiate(store.as_context_mut())?;
    let bindings = DuckdbExtension::new(store.as_context_mut(), &instance)?;
    bindings
        .duckdb_extension_guest()
        .call_load(store.as_context_mut())?
        .map_err(|err| {
            wasmtime::Error::msg(format!(
                "extension component '{extension_name}' returned error from load(): {err:?}"
            ))
        })?;
    Ok(ExtensionInstance::new(store, bindings, instance))
}
