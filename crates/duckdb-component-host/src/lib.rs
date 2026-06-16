pub mod duckdb_core_bindings {
    wasmtime::component::bindgen!({
        path: "../../crates/duckdb-core-component/wit",
        world: "duckdb:component/libduckdb",
        with: {
            "wasi:cli/environment": wasmtime_wasi::p2::bindings::cli::environment,
            "wasi:filesystem/preopens": wasmtime_wasi::p2::bindings::filesystem::preopens,
            "wasi:filesystem/types": wasmtime_wasi::p2::bindings::filesystem::types,
            "wasi:io/streams": wasmtime_wasi::p2::bindings::io::streams,
        },
        require_store_data_send: true,
    });
}

pub mod duckdb_cli_bindings {
    wasmtime::component::bindgen!({
        path: "../../crates/duckdb-cli-component/wit",
        world: "duckdb:cli/duckdb-cli",
        with: {
            "wasi:cli/environment": wasmtime_wasi::p2::bindings::cli::environment,
            "wasi:cli/stdin": wasmtime_wasi::p2::bindings::cli::stdin,
            "wasi:cli/stdout": wasmtime_wasi::p2::bindings::cli::stdout,
            "wasi:cli/stderr": wasmtime_wasi::p2::bindings::cli::stderr,
            "wasi:filesystem/preopens": wasmtime_wasi::p2::bindings::filesystem::preopens,
        },
        require_store_data_send: true,
    });
}

use std::collections::{BTreeMap, HashMap};
#[cfg(test)]
use std::fs;
use std::path::{Path, PathBuf};
#[cfg(test)]
use std::process::Command;
use std::sync::{Arc, Mutex, OnceLock};
use std::thread;

use anyhow::{Context, Result};
use duckdb_cli_bindings::duckdb::component::database as cli_db;
use duckdb_cli_bindings::duckdb::extension::types as cli_types;
use duckdb_core_bindings::duckdb::component::extension_loader_hooks as core_extension_hooks;
use duckdb_core_bindings::duckdb::component::host_extension_loader as core_host_loader;
use duckdb_core_bindings::duckdb::extension::callback_dispatch as core_callback_dispatch;
use duckdb_core_bindings::duckdb::extension::types as core_types;
use duckdb_core_bindings::exports::duckdb::component::database as core_db_exports;
use duckdb_core_bindings::exports::duckdb::extension::{
    config as core_config_exports, logging as core_logging_exports, runtime as core_runtime_exports,
};
use duckdb_extension_bindings::duckdb::extension::{
    config as extension_config, logging as extension_logging, runtime as extension_runtime,
    types as extension_types,
};
use duckdb_extension_bindings::{DuckdbExtension, DuckdbExtensionPre};
use wasmtime::component::__internal::Vec as BindgenVec;
use wasmtime::component::{Component, Linker, Resource, ResourceAny, ResourceTable};
use wasmtime::{AsContextMut, Config, Engine, Store, StoreContextMut};
use wasmtime_wasi::p2::{
    self,
    pipe::{MemoryInputPipe, MemoryOutputPipe},
};
use wasmtime_wasi::{DirPerms, FilePerms, WasiCtx, WasiCtxBuilder, WasiCtxView, WasiView};

type CliString = wasmtime::component::__internal::String;

struct CoreStoreState {
    table: ResourceTable,
    wasi: WasiCtx,
    extension_manager: Arc<Mutex<ExtensionManager>>,
}

impl WasiView for CoreStoreState {
    fn ctx(&mut self) -> WasiCtxView<'_> {
        WasiCtxView {
            ctx: &mut self.wasi,
            table: &mut self.table,
        }
    }
}

impl wasmtime::component::HasData for CoreStoreState {
    type Data<'a> = &'a mut CoreStoreState;
}

impl core_host_loader::Host for CoreStoreState {
    fn request_load(&mut self, name: wasmtime::component::__internal::String) -> bool {
        let mut manager = self
            .extension_manager
            .lock()
            .expect("extension manager mutex poisoned");
        match manager.ensure_extension_loaded(&name) {
            Ok(loaded) => loaded,
            Err(err) => {
                eprintln!("failed to load extension {name}: {err}");
                false
            }
        }
    }
}

impl core_extension_hooks::Host for CoreStoreState {
    fn get_pending_registrations(&mut self) -> core_extension_hooks::PendingRegistrations {
        let mut manager = self
            .extension_manager
            .lock()
            .expect("extension manager mutex poisoned");
        convert_pending_registrations(manager.drain_pending_registrations())
    }
}

impl core_callback_dispatch::Host for CoreStoreState {
    fn call_scalar(
        &mut self,
        handle: u32,
        args: BindgenVec<core_types::Duckvalue>,
        ctx: core_callback_dispatch::Invokeinfo,
    ) -> Result<core_types::Duckvalue, core_types::Duckerror> {
        let converted_args: Vec<_> = args
            .into_iter()
            .map(convert_core_duckvalue_to_extension)
            .collect();
        let converted_ctx = convert_core_invokeinfo(ctx);
        let mut manager = self
            .extension_manager
            .lock()
            .expect("extension manager mutex poisoned");
        manager
            .dispatch_scalar(handle, converted_args.as_slice(), converted_ctx)
            .map(convert_extension_duckvalue_to_core)
            .map_err(convert_extension_duckerror_to_core)
    }

    fn call_table(
        &mut self,
        handle: u32,
        args: BindgenVec<core_types::Duckvalue>,
    ) -> Result<core_callback_dispatch::Resultset, core_types::Duckerror> {
        let converted_args: Vec<_> = args
            .into_iter()
            .map(convert_core_duckvalue_to_extension)
            .collect();
        let mut manager = self
            .extension_manager
            .lock()
            .expect("extension manager mutex poisoned");
        manager
            .dispatch_table(handle, converted_args.as_slice())
            .map(convert_extension_resultset_to_core)
            .map_err(convert_extension_duckerror_to_core)
    }

    fn call_aggregate(
        &mut self,
        handle: u32,
        rows: core_callback_dispatch::Rowbatch,
    ) -> Result<core_types::Duckvalue, core_types::Duckerror> {
        let converted_rows = convert_core_rowbatch_to_extension(rows);
        let mut manager = self
            .extension_manager
            .lock()
            .expect("extension manager mutex poisoned");
        manager
            .dispatch_aggregate(handle, &converted_rows)
            .map(convert_extension_duckvalue_to_core)
            .map_err(convert_extension_duckerror_to_core)
    }

    fn call_pragma(
        &mut self,
        handle: u32,
        args: BindgenVec<core_types::Duckvalue>,
    ) -> Result<Option<core_types::Duckvalue>, core_types::Duckerror> {
        let converted_args: Vec<_> = args
            .into_iter()
            .map(convert_core_duckvalue_to_extension)
            .collect();
        let mut manager = self
            .extension_manager
            .lock()
            .expect("extension manager mutex poisoned");
        manager
            .dispatch_pragma(handle, converted_args.as_slice())
            .map(|result| result.map(convert_extension_duckvalue_to_core))
            .map_err(convert_extension_duckerror_to_core)
    }
}

struct CoreExecution {
    store: Store<CoreStoreState>,
    bindings: duckdb_core_bindings::Libduckdb,
}

impl CoreExecution {
    fn with_database<F, R>(&mut self, f: F) -> R
    where
        F: FnOnce(&core_db_exports::Guest, wasmtime::StoreContextMut<'_, CoreStoreState>) -> R,
    {
        let guest = self.bindings.duckdb_component_database();
        let store = self.store.as_context_mut();
        f(guest, store)
    }

    fn with_stream<F, R>(&mut self, f: F) -> R
    where
        F: FnOnce(
            core_db_exports::GuestResultStream<'_>,
            wasmtime::StoreContextMut<'_, CoreStoreState>,
        ) -> R,
    {
        let guest = self.bindings.duckdb_component_database().result_stream();
        let store = self.store.as_context_mut();
        f(guest, store)
    }

    fn with_runtime<F, R>(&mut self, f: F) -> R
    where
        F: FnOnce(&core_runtime_exports::Guest, wasmtime::StoreContextMut<'_, CoreStoreState>) -> R,
    {
        let guest = self.bindings.duckdb_extension_runtime();
        let store = self.store.as_context_mut();
        f(guest, store)
    }

    fn with_config<F, R>(&mut self, f: F) -> R
    where
        F: FnOnce(&core_config_exports::Guest, wasmtime::StoreContextMut<'_, CoreStoreState>) -> R,
    {
        let guest = self.bindings.duckdb_extension_config();
        let store = self.store.as_context_mut();
        f(guest, store)
    }

    fn with_logging<F, R>(&mut self, f: F) -> R
    where
        F: FnOnce(&core_logging_exports::Guest, wasmtime::StoreContextMut<'_, CoreStoreState>) -> R,
    {
        let guest = self.bindings.duckdb_extension_logging();
        let store = self.store.as_context_mut();
        f(guest, store)
    }
}

struct ConnectionEntry {
    handle: ResourceAny,
    closed: bool,
}

struct StreamEntry {
    handle: ResourceAny,
    closed: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum CallbackKind {
    Scalar,
    Table,
    Aggregate,
    Pragma,
}

#[derive(Clone, Debug)]
struct CallbackEntry {
    extension: String,
    dispatcher_handle: u32,
    kind: CallbackKind,
}

#[derive(Default)]
struct CallbackRegistry {
    next_handle: u32,
    entries: HashMap<u32, CallbackEntry>,
}

impl CallbackRegistry {
    fn new() -> Self {
        Self {
            next_handle: 1,
            entries: HashMap::new(),
        }
    }

    fn allocate(&mut self, extension: &str, kind: CallbackKind, dispatcher_handle: u32) -> u32 {
        let handle = self.next_handle;
        self.next_handle = self.next_handle.wrapping_add(1).max(1);
        self.entries.insert(
            handle,
            CallbackEntry {
                extension: extension.to_string(),
                dispatcher_handle,
                kind,
            },
        );
        eprintln!(
            "[extension-manager] registered {} callback handle {} for '{}' (dispatcher={dispatcher_handle})",
            describe_callback_kind(kind),
            handle,
            extension
        );
        handle
    }

    fn remove(&mut self, handle: u32) {
        if let Some(entry) = self.entries.remove(&handle) {
            eprintln!(
                "[extension-manager] released {} callback handle {} for '{}'",
                describe_callback_kind(entry.kind),
                handle,
                entry.extension
            );
        }
    }

    fn remove_extension(&mut self, extension: &str) {
        let initial = self.entries.len();
        self.entries.retain(|_, entry| entry.extension != extension);
        let removed = initial.saturating_sub(self.entries.len());
        if removed > 0 {
            eprintln!(
                "[extension-manager] purged {removed} callback handles after unloading '{}'",
                extension
            );
        }
    }

    fn get(&self, handle: u32) -> Option<CallbackEntry> {
        self.entries.get(&handle).cloned()
    }
}

struct ExtensionInstance {
    store: Store<ExtensionStoreState>,
    bindings: DuckdbExtension,
}

impl ExtensionInstance {
    fn dispatch_scalar(
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
            .map_err(|err| err)
    }

    fn dispatch_table(
        &mut self,
        dispatcher_handle: u32,
        args: &[extension_types::Duckvalue],
    ) -> Result<extension_runtime::Resultset, extension_types::Duckerror> {
        let guest = self.bindings.duckdb_extension_callback_dispatch();
        let mut store = self.store.as_context_mut();
        guest
            .call_call_table(&mut store, dispatcher_handle, args)
            .map_err(map_extension_trap)?
            .map_err(|err| err)
    }

    fn dispatch_aggregate(
        &mut self,
        dispatcher_handle: u32,
        rows: &extension_runtime::Rowbatch,
    ) -> Result<extension_types::Duckvalue, extension_types::Duckerror> {
        let guest = self.bindings.duckdb_extension_callback_dispatch();
        let mut store = self.store.as_context_mut();
        guest
            .call_call_aggregate(&mut store, dispatcher_handle, rows)
            .map_err(map_extension_trap)?
            .map_err(|err| err)
    }

    fn dispatch_pragma(
        &mut self,
        dispatcher_handle: u32,
        args: &[extension_types::Duckvalue],
    ) -> Result<Option<extension_types::Duckvalue>, extension_types::Duckerror> {
        let guest = self.bindings.duckdb_extension_callback_dispatch();
        let mut store = self.store.as_context_mut();
        guest
            .call_call_pragma(&mut store, dispatcher_handle, args)
            .map_err(map_extension_trap)?
            .map_err(|err| err)
    }

    fn drain_pending(&mut self) -> PendingRegistrationsData {
        let mut ctx = self.store.as_context_mut();
        let data: *mut ExtensionStoreState = ctx.data_mut();
        unsafe { (*data).drain_pending() }
    }
}

struct ExtensionManager {
    engine: Engine,
    core: Option<Arc<Mutex<CoreExecution>>>,
    extensions: HashMap<String, ExtensionInstance>,
    callback_registry: Arc<Mutex<CallbackRegistry>>,
}

impl ExtensionManager {
    fn new(engine: Engine) -> Self {
        Self {
            engine,
            core: None,
            extensions: HashMap::new(),
            callback_registry: Arc::new(Mutex::new(CallbackRegistry::new())),
        }
    }

    fn attach_core(&mut self, core: Arc<Mutex<CoreExecution>>) {
        self.core = Some(core);
    }

    fn dispatch_scalar(
        &mut self,
        handle: u32,
        args: &[extension_types::Duckvalue],
        ctx: extension_runtime::Invokeinfo,
    ) -> Result<extension_types::Duckvalue, extension_types::Duckerror> {
        let entry = {
            let registry = self
                .callback_registry
                .lock()
                .expect("callback registry mutex poisoned");
            match registry.get(handle) {
                Some(entry) if entry.kind == CallbackKind::Scalar => entry,
                Some(entry) => {
                    eprintln!(
                        "[extension-manager] callback handle {handle} expected scalar but is {:?}",
                        entry.kind
                    );
                    return Err(extension_types::Duckerror::Invalidstate(format!(
                        "callback handle {handle} is not scalar"
                    )));
                }
                None => {
                    eprintln!(
                        "[extension-manager] dispatch_scalar received unknown handle {handle}"
                    );
                    return Err(extension_types::Duckerror::Invalidstate(format!(
                        "unknown scalar callback handle {handle}"
                    )));
                }
            }
        };
        let instance = match self.extensions.get_mut(&entry.extension) {
            Some(instance) => instance,
            None => {
                eprintln!(
                    "[extension-manager] dispatch_scalar could not find loaded extension '{}'",
                    entry.extension
                );
                return Err(extension_types::Duckerror::Invalidstate(format!(
                    "extension {} is not loaded",
                    entry.extension
                )));
            }
        };
        instance.dispatch_scalar(entry.dispatcher_handle, args, ctx)
    }

    fn dispatch_table(
        &mut self,
        handle: u32,
        args: &[extension_types::Duckvalue],
    ) -> Result<extension_runtime::Resultset, extension_types::Duckerror> {
        let entry = match self.lookup_callback(handle, CallbackKind::Table) {
            Some(entry) => entry,
            None => {
                eprintln!(
                    "[extension-manager] dispatch_table received unknown handle {handle}"
                );
                return Err(extension_types::Duckerror::Invalidstate(format!(
                    "unknown table callback handle {handle}"
                )));
            }
        };
        let instance = match self.extensions.get_mut(&entry.extension) {
            Some(instance) => instance,
            None => {
                eprintln!(
                    "[extension-manager] dispatch_table could not find loaded extension '{}'",
                    entry.extension
                );
                return Err(extension_types::Duckerror::Invalidstate(format!(
                    "extension {} is not loaded",
                    entry.extension
                )));
            }
        };
        instance.dispatch_table(entry.dispatcher_handle, args)
    }

    fn dispatch_aggregate(
        &mut self,
        handle: u32,
        rows: &extension_runtime::Rowbatch,
    ) -> Result<extension_types::Duckvalue, extension_types::Duckerror> {
        let entry = match self.lookup_callback(handle, CallbackKind::Aggregate) {
            Some(entry) => entry,
            None => {
                eprintln!(
                    "[extension-manager] dispatch_aggregate received unknown handle {handle}"
                );
                return Err(extension_types::Duckerror::Invalidstate(format!(
                    "unknown aggregate callback handle {handle}"
                )));
            }
        };
        let instance = match self.extensions.get_mut(&entry.extension) {
            Some(instance) => instance,
            None => {
                eprintln!(
                    "[extension-manager] dispatch_aggregate could not find loaded extension '{}'",
                    entry.extension
                );
                return Err(extension_types::Duckerror::Invalidstate(format!(
                    "extension {} is not loaded",
                    entry.extension
                )));
            }
        };
        instance.dispatch_aggregate(entry.dispatcher_handle, rows)
    }

    fn dispatch_pragma(
        &mut self,
        handle: u32,
        args: &[extension_types::Duckvalue],
    ) -> Result<Option<extension_types::Duckvalue>, extension_types::Duckerror> {
        let entry = match self.lookup_callback(handle, CallbackKind::Pragma) {
            Some(entry) => entry,
            None => {
                eprintln!(
                    "[extension-manager] dispatch_pragma received unknown handle {handle}"
                );
                return Err(extension_types::Duckerror::Invalidstate(format!(
                    "unknown pragma callback handle {handle}"
                )));
            }
        };
        let instance = match self.extensions.get_mut(&entry.extension) {
            Some(instance) => instance,
            None => {
                eprintln!(
                    "[extension-manager] dispatch_pragma could not find loaded extension '{}'",
                    entry.extension
                );
                return Err(extension_types::Duckerror::Invalidstate(format!(
                    "extension {} is not loaded",
                    entry.extension
                )));
            }
        };
        instance.dispatch_pragma(entry.dispatcher_handle, args)
    }

    fn lookup_callback(&self, handle: u32, kind: CallbackKind) -> Option<CallbackEntry> {
        let registry = self
            .callback_registry
            .lock()
            .expect("callback registry mutex poisoned");
        registry.get(handle).filter(|entry| entry.kind == kind)
    }

    fn ensure_extension_loaded(&mut self, name: &str) -> wasmtime::Result<bool> {
        let sanitized = sanitize_extension_name(name);
        if self.extensions.contains_key(&sanitized) {
            return Ok(true);
        }

        let artifact = extension_artifact_path(&sanitized);
        if !artifact.exists() {
            eprintln!(
                "[extension-manager] no artifact found for '{sanitized}' at {}; skipping load request",
                artifact.display()
            );
            return Ok(false);
        }

        let core = match self.core.as_ref() {
            Some(core) => core.clone(),
            None => {
                eprintln!(
                    "extension load requested before core execution was attached; skipping {sanitized}"
                );
                return Ok(false);
            }
        };

        let engine = self.engine.clone();
        let artifact_path = artifact.clone();
        let callback_registry = self.callback_registry.clone();
        let extension_name = sanitized.clone();
        eprintln!(
            "[extension-manager] attempting to load '{sanitized}' from {}",
            artifact_path.display()
        );
        let handle = thread::spawn(move || -> wasmtime::Result<ExtensionInstance> {
            let wasi = WasiCtxBuilder::new().inherit_env().inherit_stdio().build();
            let mut store = Store::new(
                &engine,
                ExtensionStoreState::new(wasi, core, callback_registry, extension_name.clone()),
            );
            let mut linker = Linker::<ExtensionStoreState>::new(&engine);
            p2::add_to_linker_sync(&mut linker)?;
            extension_types::add_to_linker::<ExtensionStoreState, ExtensionStoreState>(
                &mut linker,
                |state| state,
            )?;
            extension_runtime::add_to_linker::<ExtensionStoreState, ExtensionStoreState>(
                &mut linker,
                |state| state,
            )?;
            extension_config::add_to_linker::<ExtensionStoreState, ExtensionStoreState>(
                &mut linker,
                |state| state,
            )?;
            extension_logging::add_to_linker::<ExtensionStoreState, ExtensionStoreState>(
                &mut linker,
                |state| state,
            )?;

            let component = Component::from_file(&engine, &artifact_path).map_err(|err| {
                wasmtime::Error::msg(format!(
                    "failed to load component for {extension_name} at {}: {err}",
                    artifact_path.display()
                ))
            })?;
            let instance_pre = linker.instantiate_pre(&component).map_err(|err| {
                wasmtime::Error::msg(format!(
                    "failed to instantiate extension linker for {extension_name}: {err}"
                ))
            })?;
            let pre = DuckdbExtensionPre::new(instance_pre).map_err(|err| {
                wasmtime::Error::msg(format!(
                    "failed to prepare extension {extension_name}: {err}"
                ))
            })?;
            let bindings = pre.instantiate(store.as_context_mut()).map_err(|err| {
                wasmtime::Error::msg(format!(
                    "failed to instantiate extension store for {extension_name}: {err}"
                ))
            })?;
            let result = bindings
                .duckdb_extension_guest()
                .call_load(store.as_context_mut())
                .map_err(|err| err)?;
            match result {
                Ok(_) => Ok(ExtensionInstance { store, bindings }),
                Err(err) => Err(wasmtime::Error::msg(format!(
                    "extension component returned error for {extension_name}: {err:?}"
                ))),
            }
        });

        let instance = match handle.join() {
            Ok(result) => match result {
                Ok(instance) => instance,
                Err(err) => {
                    eprintln!("extension instantiation for {sanitized} failed: {err}");
                    return Err(err);
                }
            },
            Err(err) => {
                return Err(wasmtime::Error::msg(format!(
                    "extension loader thread panicked: {err:?}"
                )))
            }
        };
        let loaded_name = sanitized.clone();
        self.extensions.insert(sanitized, instance);
        eprintln!(
            "[extension-manager] extension '{loaded_name}' loaded successfully and ready for registrations"
        );
        Ok(true)
    }

    fn is_loaded(&self, name: &str) -> bool {
        let sanitized = sanitize_extension_name(name);
        self.extensions.contains_key(&sanitized)
    }

    fn drain_pending_registrations(&mut self) -> PendingRegistrationsData {
        let mut aggregated = PendingRegistrationsData::default();
        for instance in self.extensions.values_mut() {
            aggregated.append(instance.drain_pending());
        }
        let scalar_names =
            summarize_registration_names(&aggregated.scalars, |entry| entry.name.as_str());
        let table_names =
            summarize_registration_names(&aggregated.tables, |entry| entry.name.as_str());
        let aggregate_names =
            summarize_registration_names(&aggregated.aggregates, |entry| entry.name.as_str());
        eprintln!(
            "[extension-manager] aggregated pending registrations: scalars={} ({scalar_names}), tables={} ({table_names}), aggregates={} ({aggregate_names})",
            aggregated.scalars.len(),
            aggregated.tables.len(),
            aggregated.aggregates.len()
        );
        aggregated
    }
}
struct ExtensionStoreState {
    table: ResourceTable,
    wasi: WasiCtx,
    core: Arc<Mutex<CoreExecution>>,
    next_resource_id: u32,
    scalar_registries: HashMap<u32, PendingScalarRegistry>,
    table_registries: HashMap<u32, PendingTableRegistry>,
    aggregate_registries: HashMap<u32, PendingAggregateRegistry>,
    // Registrations are retained here once their registry resource is dropped by
    // the guest (which happens as soon as `load()` returns), so they survive
    // until `drain_pending` forwards them to the core component.
    pending_scalars: Vec<PendingScalar>,
    pending_tables: Vec<PendingTable>,
    pending_aggregates: Vec<PendingAggregate>,
    callback_registry: Arc<Mutex<CallbackRegistry>>,
    extension_name: String,
}

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

struct PendingScalar {
    extension: String,
    name: String,
    arguments: Vec<core_runtime_exports::Funcarg>,
    returns: core_runtime_exports::Logicaltype,
    callback_handle: u32,
    options: Option<core_runtime_exports::Funcopts>,
}

struct PendingTable {
    extension: String,
    name: String,
    arguments: Vec<core_runtime_exports::Funcarg>,
    columns: Vec<core_runtime_exports::Columndef>,
    callback_handle: u32,
    options: Option<core_runtime_exports::Extopts>,
}

struct PendingAggregate {
    extension: String,
    name: String,
    arguments: Vec<core_runtime_exports::Funcarg>,
    returns: core_runtime_exports::Logicaltype,
    callback_handle: u32,
    options: Option<core_runtime_exports::Funcopts>,
}

#[derive(Default)]
struct PendingRegistrationsData {
    scalars: Vec<PendingScalar>,
    tables: Vec<PendingTable>,
    aggregates: Vec<PendingAggregate>,
}

impl PendingRegistrationsData {
    fn append(&mut self, mut other: PendingRegistrationsData) {
        self.scalars.append(&mut other.scalars);
        self.tables.append(&mut other.tables);
        self.aggregates.append(&mut other.aggregates);
    }
}

fn summarize_registration_names<T, F>(entries: &[T], mut project: F) -> String
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

impl ExtensionStoreState {
    fn new(
        wasi: WasiCtx,
        core: Arc<Mutex<CoreExecution>>,
        callback_registry: Arc<Mutex<CallbackRegistry>>,
        extension_name: String,
    ) -> Self {
        Self {
            table: ResourceTable::new(),
            wasi,
            core,
            next_resource_id: 1,
            scalar_registries: HashMap::new(),
            table_registries: HashMap::new(),
            aggregate_registries: HashMap::new(),
            pending_scalars: Vec::new(),
            pending_tables: Vec::new(),
            pending_aggregates: Vec::new(),
            callback_registry,
            extension_name,
        }
    }

    fn with_core<F, R>(&self, f: F) -> R
    where
        F: FnOnce(&mut CoreExecution) -> R,
    {
        let mut core = self.core.lock().expect("core mutex poisoned");
        f(&mut core)
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
        let pending = PendingRegistrationsData {
            scalars,
            tables,
            aggregates,
        };
        let scalar_names = summarize_registration_names(&pending.scalars, |entry| entry.name.as_str());
        let table_names =
            summarize_registration_names(&pending.tables, |entry| entry.name.as_str());
        let aggregate_names = summarize_registration_names(&pending.aggregates, |entry| entry.name.as_str());
        eprintln!(
            "[extension-runtime:{}] draining pending registrations: scalars={} ({scalar_names}), tables={} ({table_names}), aggregates={} ({aggregate_names})",
            self.extension_name,
            pending.scalars.len(),
            pending.tables.len(),
            pending.aggregates.len()
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

pub struct HostState {
    table: ResourceTable,
    wasi: WasiCtx,
    core: Arc<Mutex<CoreExecution>>,
    extension_manager: Arc<Mutex<ExtensionManager>>,
    next_resource_id: u32,
    connections: HashMap<u32, ConnectionEntry>,
    streams: HashMap<u32, StreamEntry>,
    pending_connection_drops: Vec<Resource<cli_db::Connection>>,
    pending_stream_drops: Vec<Resource<cli_db::ResultStream>>,
}

impl WasiView for HostState {
    fn ctx(&mut self) -> WasiCtxView<'_> {
        WasiCtxView {
            ctx: &mut self.wasi,
            table: &mut self.table,
        }
    }
}

impl wasmtime::component::HasData for HostState {
    type Data<'a> = &'a mut HostState;
}

impl HostState {
    fn alloc_resource_id(&mut self) -> u32 {
        let id = self.next_resource_id;
        self.next_resource_id = self.next_resource_id.wrapping_add(1).max(1);
        id
    }

    fn with_core<F, R>(&self, f: F) -> R
    where
        F: FnOnce(&mut CoreExecution) -> R,
    {
        let mut core = self.core.lock().expect("core mutex poisoned");
        f(&mut core)
    }

    fn drain_pending_resource_drops(&mut self) -> Result<(), cli_types::Duckerror> {
        let pending_conn = std::mem::take(&mut self.pending_connection_drops);
        for conn in pending_conn {
            self.drop_connection_resource(conn)?;
        }
        let pending_streams = std::mem::take(&mut self.pending_stream_drops);
        for stream in pending_streams {
            self.drop_stream_resource(stream)?;
        }
        Ok(())
    }

    fn drop_connection_resource(
        &mut self,
        conn: Resource<cli_db::Connection>,
    ) -> Result<(), cli_types::Duckerror> {
        if let Some(entry) = self.connections.remove(&conn.rep()) {
            if !entry.closed {
                self.with_core(|core| {
                    core.with_database(|guest, store| guest.call_close(store, entry.handle))
                })
                .map_err(|err| cli_types::Duckerror::Internal(trap_to_cli_string(err)))?;
            }
        }
        Ok(())
    }

    fn drop_stream_resource(
        &mut self,
        rep: Resource<cli_db::ResultStream>,
    ) -> Result<(), cli_types::Duckerror> {
        if let Some(entry) = self.streams.remove(&rep.rep()) {
            if !entry.closed {
                self.with_core(|core| {
                    core.with_stream(|guest, store| guest.call_close(store, entry.handle))
                })
                .map_err(|err| cli_types::Duckerror::Internal(trap_to_cli_string(err)))?;
            }
        }
        Ok(())
    }

    fn preload_extension(&mut self, name: &str) -> wasmtime::Result<()> {
        let mut manager = self
            .extension_manager
            .lock()
            .expect("extension manager mutex poisoned");
        match manager.ensure_extension_loaded(name) {
            Ok(_) => Ok(()),
            Err(err) => {
                eprintln!("failed to preload extension {name}: {err}");
                Err(err)
            }
        }
    }

    fn request_extension_load(&mut self, name: &str) -> wasmtime::Result<bool> {
        let mut manager = self
            .extension_manager
            .lock()
            .expect("extension manager mutex poisoned");
        match manager.ensure_extension_loaded(name) {
            Ok(loaded) => Ok(loaded),
            Err(err) => {
                eprintln!("request_load error for {name}: {err}");
                Err(err)
            }
        }
    }

    fn schedule_connection_drop(&mut self, conn: Resource<cli_db::Connection>) {
        self.pending_connection_drops.push(conn);
    }

    fn schedule_stream_drop(&mut self, stream: Resource<cli_db::ResultStream>) {
        self.pending_stream_drops.push(stream);
    }
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
    fn new(
        &mut self,
        handle: u32,
    ) -> wasmtime::component::Resource<extension_runtime::ScalarCallback> {
        let id = self.allocate_callback_handle(handle, CallbackKind::Scalar);
        wasmtime::component::Resource::new_own(id)
    }

    fn call(
        &mut self,
        _self_: wasmtime::component::Resource<extension_runtime::ScalarCallback>,
        _args: BindgenVec<extension_types::Duckvalue>,
        _ctx: extension_runtime::Invokeinfo,
    ) -> Result<extension_types::Duckvalue, extension_types::Duckerror> {
        Err(unsupported_runtime_error())
    }

    fn drop(
        &mut self,
        rep: wasmtime::component::Resource<extension_runtime::ScalarCallback>,
    ) -> wasmtime::Result<()> {
        self.release_callback_handle(rep.rep());
        Ok(())
    }
}

impl extension_runtime::HostTableCallback for ExtensionStoreState {
    fn new(
        &mut self,
        handle: u32,
    ) -> wasmtime::component::Resource<extension_runtime::TableCallback> {
        let id = self.allocate_callback_handle(handle, CallbackKind::Table);
        wasmtime::component::Resource::new_own(id)
    }

    fn call(
        &mut self,
        _self_: wasmtime::component::Resource<extension_runtime::TableCallback>,
        _args: BindgenVec<extension_types::Duckvalue>,
    ) -> Result<extension_runtime::Resultset, extension_types::Duckerror> {
        Err(unsupported_runtime_error())
    }

    fn drop(
        &mut self,
        rep: wasmtime::component::Resource<extension_runtime::TableCallback>,
    ) -> wasmtime::Result<()> {
        self.release_callback_handle(rep.rep());
        Ok(())
    }
}

impl extension_runtime::HostAggregateCallback for ExtensionStoreState {
    fn new(
        &mut self,
        handle: u32,
    ) -> wasmtime::component::Resource<extension_runtime::AggregateCallback> {
        let id = self.allocate_callback_handle(handle, CallbackKind::Aggregate);
        wasmtime::component::Resource::new_own(id)
    }

    fn call(
        &mut self,
        _self_: wasmtime::component::Resource<extension_runtime::AggregateCallback>,
        _rows: extension_runtime::Rowbatch,
    ) -> Result<extension_types::Duckvalue, extension_types::Duckerror> {
        Err(unsupported_runtime_error())
    }

    fn drop(
        &mut self,
        rep: wasmtime::component::Resource<extension_runtime::AggregateCallback>,
    ) -> wasmtime::Result<()> {
        self.release_callback_handle(rep.rep());
        Ok(())
    }
}

impl extension_runtime::HostPragmaCallback for ExtensionStoreState {
    fn new(
        &mut self,
        handle: u32,
    ) -> wasmtime::component::Resource<extension_runtime::PragmaCallback> {
        let id = self.allocate_callback_handle(handle, CallbackKind::Pragma);
        wasmtime::component::Resource::new_own(id)
    }

    fn call(
        &mut self,
        _self_: wasmtime::component::Resource<extension_runtime::PragmaCallback>,
        _args: BindgenVec<extension_types::Duckvalue>,
    ) -> Result<Option<extension_types::Duckvalue>, extension_types::Duckerror> {
        Err(unsupported_runtime_error())
    }

    fn drop(
        &mut self,
        rep: wasmtime::component::Resource<extension_runtime::PragmaCallback>,
    ) -> wasmtime::Result<()> {
        self.release_callback_handle(rep.rep());
        Ok(())
    }
}

impl extension_runtime::HostScalarRegistry for ExtensionStoreState {
    fn register(
        &mut self,
        self_: wasmtime::component::Resource<extension_runtime::ScalarRegistry>,
        name: String,
        arguments: BindgenVec<extension_runtime::Funcarg>,
        returns: extension_runtime::Logicaltype,
        callback: wasmtime::component::Resource<extension_runtime::ScalarCallback>,
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

    fn drop(
        &mut self,
        rep: wasmtime::component::Resource<extension_runtime::ScalarRegistry>,
    ) -> wasmtime::Result<()> {
        if let Some(registry) = self.scalar_registries.remove(&rep.rep()) {
            self.pending_scalars.extend(registry.entries);
        }
        Ok(())
    }
}

impl extension_runtime::HostTableRegistry for ExtensionStoreState {
    fn register(
        &mut self,
        self_: wasmtime::component::Resource<extension_runtime::TableRegistry>,
        name: String,
        arguments: BindgenVec<extension_runtime::Funcarg>,
        columns: BindgenVec<extension_runtime::Columndef>,
        callback: wasmtime::component::Resource<extension_runtime::TableCallback>,
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

        registry.entries.push(PendingTable {
            extension: self.extension_name.clone(),
            name,
            arguments: converted_arguments,
            columns: converted_columns,
            callback_handle,
            options: converted_options,
        });

        Ok(self.alloc_resource_id())
    }

    fn drop(
        &mut self,
        rep: wasmtime::component::Resource<extension_runtime::TableRegistry>,
    ) -> wasmtime::Result<()> {
        if let Some(registry) = self.table_registries.remove(&rep.rep()) {
            self.pending_tables.extend(registry.entries);
        }
        Ok(())
    }
}

impl extension_runtime::HostAggregateRegistry for ExtensionStoreState {
    fn register(
        &mut self,
        self_: wasmtime::component::Resource<extension_runtime::AggregateRegistry>,
        name: String,
        arguments: BindgenVec<extension_runtime::Funcarg>,
        returns: extension_runtime::Logicaltype,
        callback: wasmtime::component::Resource<extension_runtime::AggregateCallback>,
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

    fn drop(
        &mut self,
        rep: wasmtime::component::Resource<extension_runtime::AggregateRegistry>,
    ) -> wasmtime::Result<()> {
        if let Some(registry) = self.aggregate_registries.remove(&rep.rep()) {
            self.pending_aggregates.extend(registry.entries);
        }
        Ok(())
    }
}

impl extension_runtime::HostPragmaRegistry for ExtensionStoreState {
    fn register_call(
        &mut self,
        _self_: wasmtime::component::Resource<extension_runtime::PragmaRegistry>,
        _name: String,
        _arguments: BindgenVec<extension_runtime::Funcarg>,
        _returns: extension_runtime::Logicaltype,
        _callback: wasmtime::component::Resource<extension_runtime::PragmaCallback>,
        _options: Option<extension_runtime::Extopts>,
    ) -> Result<u32, extension_types::Duckerror> {
        Err(unsupported_runtime_error())
    }

    fn drop(
        &mut self,
        _rep: wasmtime::component::Resource<extension_runtime::PragmaRegistry>,
    ) -> wasmtime::Result<()> {
        Ok(())
    }
}

impl extension_runtime::HostMacroRegistry for ExtensionStoreState {
    fn register_scalar(
        &mut self,
        _self_: wasmtime::component::Resource<extension_runtime::MacroRegistry>,
        _name: String,
        _parameters: BindgenVec<String>,
        _body_sql: String,
        _options: Option<extension_runtime::Extopts>,
    ) -> Result<bool, extension_types::Duckerror> {
        Err(unsupported_runtime_error())
    }

    fn drop(
        &mut self,
        _rep: wasmtime::component::Resource<extension_runtime::MacroRegistry>,
    ) -> wasmtime::Result<()> {
        Ok(())
    }
}

impl extension_config::Host for ExtensionStoreState {
    fn provider_version(&mut self) -> String {
        self.with_core(|core| core.with_config(|guest, store| guest.call_provider_version(store)))
            .unwrap_or_else(|err| {
                eprintln!("extension config provider-version failed: {err}");
                "duckdb-extension-host".into()
            })
    }

    fn list_keys(&mut self, prefix: Option<String>) -> BindgenVec<String> {
        let prefix_ref = prefix.as_deref();
        self.with_core(|core| {
            core.with_config(|guest, store| guest.call_list_keys(store, prefix_ref))
        })
        .unwrap_or_else(|err| {
            eprintln!("extension config list-keys failed: {err}");
            Vec::new()
        })
        .into()
    }

    fn get_string(&mut self, path: String) -> Result<Option<String>, extension_types::Configerror> {
        let result = self
            .with_core(|core| core.with_config(|guest, store| guest.call_get_string(store, &path)));
        result
            .map_err(map_config_trap)?
            .map_err(convert_config_error)
    }

    fn get_bool(&mut self, path: String) -> Result<Option<bool>, extension_types::Configerror> {
        let result = self
            .with_core(|core| core.with_config(|guest, store| guest.call_get_bool(store, &path)));
        result
            .map_err(map_config_trap)?
            .map_err(convert_config_error)
    }

    fn get_i64(&mut self, path: String) -> Result<Option<i64>, extension_types::Configerror> {
        let result = self
            .with_core(|core| core.with_config(|guest, store| guest.call_get_i64(store, &path)));
        result
            .map_err(map_config_trap)?
            .map_err(convert_config_error)
    }

    fn get_u64(&mut self, path: String) -> Result<Option<u64>, extension_types::Configerror> {
        let result = self
            .with_core(|core| core.with_config(|guest, store| guest.call_get_u64(store, &path)));
        result
            .map_err(map_config_trap)?
            .map_err(convert_config_error)
    }

    fn get_f64(&mut self, path: String) -> Result<Option<f64>, extension_types::Configerror> {
        let result = self
            .with_core(|core| core.with_config(|guest, store| guest.call_get_f64(store, &path)));
        result
            .map_err(map_config_trap)?
            .map_err(convert_config_error)
    }

    fn get_bytes(
        &mut self,
        path: String,
    ) -> Result<Option<BindgenVec<u8>>, extension_types::Configerror> {
        let result = self
            .with_core(|core| core.with_config(|guest, store| guest.call_get_bytes(store, &path)));
        let value = result
            .map_err(map_config_trap)?
            .map_err(convert_config_error)?;
        Ok(value.map(|bytes| bytes.into()))
    }

    fn get_string_list(
        &mut self,
        path: String,
    ) -> Result<Option<BindgenVec<String>>, extension_types::Configerror> {
        let result = self.with_core(|core| {
            core.with_config(|guest, store| guest.call_get_string_list(store, &path))
        });
        let value = result
            .map_err(map_config_trap)?
            .map_err(convert_config_error)?;
        Ok(value.map(|items| items.into()))
    }
}

impl extension_logging::Host for ExtensionStoreState {
    fn log(&mut self, level: extension_logging::Loglevel, message: String, target: Option<String>) {
        let result = self.with_core(|core| {
            core.with_logging(|guest, store| {
                guest.call_log(
                    store,
                    convert_log_level_to_core(level),
                    &message,
                    target.as_deref(),
                )
            })
        });
        if let Err(err) = result {
            match target {
                Some(t) => {
                    eprintln!("[duckdb-extension:{level:?}:{t}] {message} (core log failed: {err})")
                }
                None => {
                    eprintln!("[duckdb-extension:{level:?}] {message} (core log failed: {err})")
                }
            }
        }
    }

    fn log_fields(
        &mut self,
        level: extension_logging::Loglevel,
        message: String,
        fields: BindgenVec<extension_logging::Logfield>,
    ) {
        let converted_fields = convert_log_fields(
            fields
                .into_iter()
                .collect::<Vec<extension_logging::Logfield>>(),
        );
        let result = self.with_core(|core| {
            core.with_logging(|guest, store| {
                guest.call_log_fields(
                    store,
                    convert_log_level_to_core(level),
                    &message,
                    converted_fields.as_slice(),
                )
            })
        });
        if let Err(err) = result {
            eprintln!("[duckdb-extension:{level:?}] {message} (core log_fields failed: {err})");
        }
    }
}

impl cli_db::HostConnection for HostState {
    fn drop(&mut self, rep: Resource<cli_db::Connection>) -> wasmtime::Result<()> {
        self.schedule_connection_drop(rep);
        Ok(())
    }
}

impl cli_db::HostResultStream for HostState {
    fn schema(
        &mut self,
        rep: Resource<cli_db::ResultStream>,
    ) -> wasmtime::component::__internal::Vec<cli_db::Columndef> {
        let entry = self
            .streams
            .get(&rep.rep())
            .unwrap_or_else(|| panic!("unknown stream handle {}", rep.rep()));
        let columns = self
            .with_core(|core| {
                core.with_stream(|guest, store| guest.call_schema(store, entry.handle.clone()))
            })
            .expect("failed to fetch schema");
        columns
            .into_iter()
            .map(convert_core_columndef)
            .collect::<Vec<_>>()
            .into()
    }

    fn next(
        &mut self,
        rep: Resource<cli_db::ResultStream>,
        max_rows: u32,
    ) -> Result<Option<wasmtime::component::__internal::Vec<cli_db::Row>>, cli_types::Duckerror>
    {
        let entry = self
            .streams
            .get(&rep.rep())
            .ok_or_else(|| cli_types::Duckerror::Internal("unknown stream".into()))?;
        let next = self
            .with_core(|core| {
                core.with_stream(|guest, store| {
                    guest.call_next(store, entry.handle.clone(), max_rows)
                })
            })
            .map_err(convert_trap_to_duckerror)?;
        match next {
            Ok(Some(rows)) => {
                let mapped = rows
                    .into_iter()
                    .map(convert_core_row)
                    .collect::<Vec<_>>()
                    .into();
                Ok(Some(mapped))
            }
            Ok(None) => Ok(None),
            Err(err) => Err(convert_core_duckerror(err)),
        }
    }

    fn close(&mut self, rep: Resource<cli_db::ResultStream>) {
        let handle = match self.streams.get(&rep.rep()) {
            Some(entry) if !entry.closed => entry.handle.clone(),
            _ => return,
        };
        if let Err(err) =
            self.with_core(|core| core.with_stream(|guest, store| guest.call_close(store, handle)))
        {
            panic!("failed to close result stream: {err}");
        }
        if let Some(entry) = self.streams.get_mut(&rep.rep()) {
            entry.closed = true;
        }
    }

    fn drop(&mut self, rep: Resource<cli_db::ResultStream>) -> wasmtime::Result<()> {
        self.schedule_stream_drop(rep);
        Ok(())
    }
}

impl cli_db::Host for HostState {
    fn open(&mut self, path: Option<CliString>) -> Result<Resource<cli_db::Connection>, CliString> {
        let owned: Option<String> = path.map(|s| s.into());
        let result = self
            .with_core(|core| {
                core.with_database(|guest, store| guest.call_open(store, owned.as_deref()))
            })
            .map_err(trap_to_cli_string)?;
        match result {
            Ok(handle) => {
                let id = self.alloc_resource_id();
                self.connections.insert(
                    id,
                    ConnectionEntry {
                        handle,
                        closed: false,
                    },
                );
                Ok(Resource::new_own(id))
            }
            Err(err) => Err(err),
        }
    }

    fn close(&mut self, conn: Resource<cli_db::Connection>) {
        let handle = match self.connections.get(&conn.rep()) {
            Some(entry) if !entry.closed => entry.handle.clone(),
            _ => return,
        };
        if let Err(err) = self
            .with_core(|core| core.with_database(|guest, store| guest.call_close(store, handle)))
        {
            panic!("failed to close connection: {err}");
        }
        if let Some(entry) = self.connections.get_mut(&conn.rep()) {
            entry.closed = true;
        }
    }

    fn interrupt(&mut self, conn: Resource<cli_db::Connection>) {
        if let Some(entry) = self.connections.get(&conn.rep()) {
            if let Err(err) = self.with_core(|core| {
                core.with_database(|guest, store| guest.call_interrupt(store, entry.handle.clone()))
            }) {
                panic!("failed to interrupt connection: {err}");
            }
        }
    }

    fn execute(
        &mut self,
        conn: Resource<cli_db::Connection>,
        sql: CliString,
    ) -> Result<cli_db::QueryResult, cli_types::Duckerror> {
        let entry = self
            .connections
            .get(&conn.rep())
            .ok_or_else(|| cli_types::Duckerror::Internal("unknown connection".into()))?;
        let result = self
            .with_core(|core| {
                core.with_database(|guest, store| {
                    guest.call_execute(store, entry.handle.clone(), &sql)
                })
            })
            .map_err(convert_trap_to_duckerror)?;
        match result {
            Ok(value) => Ok(convert_core_query_result(value)),
            Err(err) => Err(convert_core_duckerror(err)),
        }
    }

    fn open_stream(
        &mut self,
        conn: Resource<cli_db::Connection>,
        sql: CliString,
    ) -> Result<Resource<cli_db::ResultStream>, cli_types::Duckerror> {
        let entry = self
            .connections
            .get(&conn.rep())
            .ok_or_else(|| cli_types::Duckerror::Internal("unknown connection".into()))?;
        let stream = self
            .with_core(|core| {
                core.with_database(|guest, store| {
                    guest.call_open_stream(store, entry.handle.clone(), &sql)
                })
            })
            .map_err(convert_trap_to_duckerror)?;
        match stream {
            Ok(handle) => {
                let id = self.alloc_resource_id();
                self.streams.insert(
                    id,
                    StreamEntry {
                        handle,
                        closed: false,
                    },
                );
                Ok(Resource::new_own(id))
            }
            Err(err) => Err(convert_core_duckerror(err)),
        }
    }

    fn register_extension(
        &mut self,
        name: CliString,
        requires: wasmtime::component::__internal::Vec<cli_types::Capabilitykind>,
    ) -> Result<bool, CliString> {
        let extension_name: String = name.clone().into();
        let requested_caps: Vec<cli_types::Capabilitykind> = requires.into_iter().collect();
        let capability_summary = summarize_cli_capabilities(requested_caps.iter().copied());
        let capability_list: Vec<core_types::Capabilitykind> = requested_caps
            .iter()
            .copied()
            .map(convert_cli_capability)
            .collect();
        eprintln!(
            "[duckdb-host] register_extension requested: name='{extension_name}', capabilities={capability_summary}"
        );
        let result = match self.with_core(|core| {
            core.with_database(|guest, store| {
                guest.call_register_extension(store, &name, capability_list.as_slice())
            })
        }) {
            Ok(result) => result,
            Err(err) => {
                eprintln!(
                    "[duckdb-host] failed to invoke core register_extension for '{extension_name}': {err}"
                );
                return Err(trap_to_cli_string(err));
            }
        };
        match result {
            Ok(value) => {
                eprintln!(
                    "[duckdb-host] core register_extension completed for '{extension_name}' (registered={value})"
                );
                Ok(value)
            }
            Err(err) => {
                let err_msg: String = err.clone().into();
                eprintln!(
                    "[duckdb-host] core register_extension rejected '{extension_name}': {err_msg}"
                );
                Err(err)
            }
        }
    }

    fn list_registered_extensions(
        &mut self,
    ) -> wasmtime::component::__internal::Vec<cli_db::ExtensionInfo> {
        let list = self
            .with_core(|core| {
                core.with_database(|guest, store| guest.call_list_registered_extensions(store))
            })
            .expect("failed to list registered extensions");
        list.into_iter()
            .map(convert_core_extension_info)
            .collect::<Vec<_>>()
            .into()
    }
}

fn convert_core_query_result(result: core_db_exports::QueryResult) -> cli_db::QueryResult {
    cli_db::QueryResult {
        columns: result
            .columns
            .into_iter()
            .map(convert_core_columndef)
            .collect(),
        rows: result.rows.into_iter().map(convert_core_row).collect(),
    }
}

fn convert_core_row(row: core_db_exports::Row) -> cli_db::Row {
    row.into_iter().map(convert_core_duckvalue).collect()
}

fn convert_core_columndef(col: core_db_exports::Columndef) -> cli_db::Columndef {
    cli_db::Columndef {
        name: col.name.into(),
        logical: convert_core_logicaltype(col.logical),
    }
}

fn convert_core_extension_info(info: core_db_exports::ExtensionInfo) -> cli_db::ExtensionInfo {
    cli_db::ExtensionInfo {
        name: info.name.into(),
        requires: info
            .requires
            .into_iter()
            .map(convert_core_capabilitykind)
            .collect(),
    }
}

fn convert_pending_registrations(
    data: PendingRegistrationsData,
) -> core_extension_hooks::PendingRegistrations {
    log_pending_batch_summary(&data);
    core_extension_hooks::PendingRegistrations {
        scalars: data
            .scalars
            .into_iter()
            .map(convert_pending_scalar_registration)
            .collect::<Vec<_>>()
            .into(),
        tables: data
            .tables
            .into_iter()
            .map(convert_pending_table_registration)
            .collect::<Vec<_>>()
            .into(),
        aggregates: data
            .aggregates
            .into_iter()
            .map(convert_pending_aggregate_registration)
            .collect::<Vec<_>>()
            .into(),
    }
}

fn convert_pending_scalar_registration(
    entry: PendingScalar,
) -> core_extension_hooks::ScalarRegistration {
    log_pending_scalar_conversion(&entry);
    core_extension_hooks::ScalarRegistration {
        name: entry.name,
        arguments: convert_funcargs_to_loader(entry.arguments),
        returns: entry.returns,
        callback_handle: entry.callback_handle,
        options: entry.options.map(convert_funcopts_to_loader),
    }
}

fn convert_pending_table_registration(
    entry: PendingTable,
) -> core_extension_hooks::TableRegistration {
    log_pending_table_conversion(&entry);
    core_extension_hooks::TableRegistration {
        name: entry.name,
        arguments: convert_funcargs_to_loader(entry.arguments),
        columns: entry.columns.into_iter().collect::<Vec<_>>().into(),
        callback_handle: entry.callback_handle,
        options: entry.options.map(convert_extopts_to_loader),
    }
}

fn convert_pending_aggregate_registration(
    entry: PendingAggregate,
) -> core_extension_hooks::AggregateRegistration {
    log_pending_aggregate_conversion(&entry);
    core_extension_hooks::AggregateRegistration {
        name: entry.name,
        arguments: convert_funcargs_to_loader(entry.arguments),
        returns: entry.returns,
        callback_handle: entry.callback_handle,
        options: entry.options.map(convert_funcopts_to_loader),
    }
}

fn convert_funcargs_to_loader(
    args: Vec<core_runtime_exports::Funcarg>,
) -> BindgenVec<core_extension_hooks::FuncArg> {
    args.into_iter()
        .map(|arg| core_extension_hooks::FuncArg {
            name: arg.name,
            logical: arg.logical,
        })
        .collect::<Vec<_>>()
        .into()
}

fn convert_funcopts_to_loader(
    opts: core_runtime_exports::Funcopts,
) -> core_extension_hooks::FuncOpts {
    core_extension_hooks::FuncOpts {
        description: opts.description,
        tags: opts.tags.into_iter().collect::<Vec<_>>().into(),
        attributes: opts.attributes,
    }
}

fn convert_extopts_to_loader(opts: core_runtime_exports::Extopts) -> core_extension_hooks::ExtOpts {
    core_extension_hooks::ExtOpts {
        description: opts.description,
        tags: opts.tags.into_iter().collect::<Vec<_>>().into(),
    }
}

fn convert_core_duckvalue(value: core_types::Duckvalue) -> cli_types::Duckvalue {
    match value {
        core_types::Duckvalue::Null => cli_types::Duckvalue::Null,
        core_types::Duckvalue::Boolean(v) => cli_types::Duckvalue::Boolean(v),
        core_types::Duckvalue::Int64(v) => cli_types::Duckvalue::Int64(v),
        core_types::Duckvalue::Uint64(v) => cli_types::Duckvalue::Uint64(v),
        core_types::Duckvalue::Float64(v) => cli_types::Duckvalue::Float64(v),
        core_types::Duckvalue::Text(v) => cli_types::Duckvalue::Text(v.into()),
        core_types::Duckvalue::Blob(v) => cli_types::Duckvalue::Blob(v.into()),
    }
}

fn convert_core_duckerror(err: core_types::Duckerror) -> cli_types::Duckerror {
    match err {
        core_types::Duckerror::Invalidargument(v) => {
            cli_types::Duckerror::Invalidargument(v.into())
        }
        core_types::Duckerror::Unsupported(v) => cli_types::Duckerror::Unsupported(v.into()),
        core_types::Duckerror::Invalidstate(v) => cli_types::Duckerror::Invalidstate(v.into()),
        core_types::Duckerror::Io(v) => cli_types::Duckerror::Io(v.into()),
        core_types::Duckerror::Internal(v) => cli_types::Duckerror::Internal(v.into()),
    }
}

fn convert_trap_to_duckerror(err: wasmtime::Error) -> cli_types::Duckerror {
    cli_types::Duckerror::Internal(err.to_string().into())
}

fn convert_core_logicaltype(ty: core_types::Logicaltype) -> cli_types::Logicaltype {
    match ty {
        core_types::Logicaltype::Boolean => cli_types::Logicaltype::Boolean,
        core_types::Logicaltype::Int64 => cli_types::Logicaltype::Int64,
        core_types::Logicaltype::Uint64 => cli_types::Logicaltype::Uint64,
        core_types::Logicaltype::Float64 => cli_types::Logicaltype::Float64,
        core_types::Logicaltype::Text => cli_types::Logicaltype::Text,
        core_types::Logicaltype::Blob => cli_types::Logicaltype::Blob,
    }
}

fn convert_core_capabilitykind(kind: core_types::Capabilitykind) -> cli_types::Capabilitykind {
    match kind {
        core_types::Capabilitykind::Scalar => cli_types::Capabilitykind::Scalar,
        core_types::Capabilitykind::Table => cli_types::Capabilitykind::Table,
        core_types::Capabilitykind::Aggregate => cli_types::Capabilitykind::Aggregate,
        core_types::Capabilitykind::Pragma => cli_types::Capabilitykind::Pragma,
        core_types::Capabilitykind::Macro => cli_types::Capabilitykind::Macro,
    }
}

fn convert_cli_capability(kind: cli_types::Capabilitykind) -> core_types::Capabilitykind {
    match kind {
        cli_types::Capabilitykind::Scalar => core_types::Capabilitykind::Scalar,
        cli_types::Capabilitykind::Table => core_types::Capabilitykind::Table,
        cli_types::Capabilitykind::Aggregate => core_types::Capabilitykind::Aggregate,
        cli_types::Capabilitykind::Pragma => core_types::Capabilitykind::Pragma,
        cli_types::Capabilitykind::Macro => core_types::Capabilitykind::Macro,
    }
}

fn summarize_cli_capabilities<I>(caps: I) -> String
where
    I: IntoIterator<Item = cli_types::Capabilitykind>,
{
    let mut parts = Vec::new();
    for cap in caps {
        parts.push(describe_cli_capability(cap));
    }
    if parts.is_empty() {
        "none".to_string()
    } else {
        parts.join(", ")
    }
}

fn describe_cli_capability(kind: cli_types::Capabilitykind) -> &'static str {
    match kind {
        cli_types::Capabilitykind::Scalar => "scalar",
        cli_types::Capabilitykind::Table => "table",
        cli_types::Capabilitykind::Aggregate => "aggregate",
        cli_types::Capabilitykind::Pragma => "pragma",
        cli_types::Capabilitykind::Macro => "macro",
    }
}

fn describe_callback_kind(kind: CallbackKind) -> &'static str {
    match kind {
        CallbackKind::Scalar => "scalar",
        CallbackKind::Table => "table",
        CallbackKind::Aggregate => "aggregate",
        CallbackKind::Pragma => "pragma",
    }
}

fn log_scalar_registration(
    extension: &str,
    name: &str,
    registry_id: u32,
    callback_handle: u32,
    args: &[core_runtime_exports::Funcarg],
    returns: &core_runtime_exports::Logicaltype,
    options: Option<&core_runtime_exports::Funcopts>,
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
    args: &[core_runtime_exports::Funcarg],
    columns: &[core_runtime_exports::Columndef],
    options: Option<&core_runtime_exports::Extopts>,
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
    args: &[core_runtime_exports::Funcarg],
    returns: &core_runtime_exports::Logicaltype,
    options: Option<&core_runtime_exports::Funcopts>,
) {
    let arg_summary = summarize_runtime_funcargs(args);
    let return_ty = describe_runtime_logicaltype(returns);
    let option_summary = summarize_funcopts(options);
    eprintln!(
        "[extension-runtime:{extension}] queued aggregate '{name}' (registry={registry_id}, callback={callback_handle}) args={arg_summary} returns={return_ty} opts={option_summary}"
    );
}

fn summarize_runtime_funcargs(args: &[core_runtime_exports::Funcarg]) -> String {
    if args.is_empty() {
        return "[]".to_string();
    }
    let parts: Vec<String> = args
        .iter()
        .map(|arg| {
            let name = arg
                .name
                .as_ref()
                .map(|s| s.as_str())
                .unwrap_or("-");
            format!("{name}:{}", describe_runtime_logicaltype(&arg.logical))
        })
        .collect();
    format!("[{}]", parts.join(", "))
}

fn summarize_runtime_columns(columns: &[core_runtime_exports::Columndef]) -> String {
    if columns.is_empty() {
        return "[]".to_string();
    }
    let parts: Vec<String> = columns
        .iter()
        .map(|col| {
            format!(
                "{}:{}",
                col.name,
                describe_runtime_logicaltype(&col.logical)
            )
        })
        .collect();
    format!("[{}]", parts.join(", "))
}

fn summarize_funcopts(options: Option<&core_runtime_exports::Funcopts>) -> String {
    match options {
        None => "none".to_string(),
        Some(opts) => {
            let description = opts
                .description
                .as_ref()
                .map(|s| s.as_str())
                .unwrap_or("-");
            let tags = if opts.tags.is_empty() {
                "none".to_string()
            } else {
                format!("[{}]", opts.tags.join(", "))
            };
            let attrs = describe_runtime_funcflags(opts.attributes);
            format!("description='{description}', tags={tags}, attrs={attrs}")
        }
    }
}

fn summarize_extopts(options: Option<&core_runtime_exports::Extopts>) -> String {
    match options {
        None => "none".to_string(),
        Some(opts) => {
            let description = opts
                .description
                .as_ref()
                .map(|s| s.as_str())
                .unwrap_or("-");
            let tags = if opts.tags.is_empty() {
                "none".to_string()
            } else {
                format!("[{}]", opts.tags.join(", "))
            };
            format!("description='{description}', tags={tags}")
        }
    }
}

fn describe_runtime_logicaltype(ty: &core_runtime_exports::Logicaltype) -> &'static str {
    match ty {
        core_runtime_exports::Logicaltype::Boolean => "BOOLEAN",
        core_runtime_exports::Logicaltype::Int64 => "INT64",
        core_runtime_exports::Logicaltype::Uint64 => "UINT64",
        core_runtime_exports::Logicaltype::Float64 => "FLOAT64",
        core_runtime_exports::Logicaltype::Text => "TEXT",
        core_runtime_exports::Logicaltype::Blob => "BLOB",
    }
}

fn describe_runtime_funcflags(flags: core_types::Funcflags) -> String {
    let mut parts = Vec::new();
    if flags.contains(core_types::Funcflags::DETERMINISTIC) {
        parts.push("deterministic");
    }
    if flags.contains(core_types::Funcflags::COMMUTATIVE) {
        parts.push("commutative");
    }
    if flags.contains(core_types::Funcflags::STATELESS) {
        parts.push("stateless");
    }
    if flags.contains(core_types::Funcflags::SIDEEFFECTING) {
        parts.push("sideeffecting");
    }
    if flags.contains(core_types::Funcflags::DEPRECATED) {
        parts.push("deprecated");
    }
    if parts.is_empty() {
        "none".to_string()
    } else {
        format!("[{}]", parts.join(", "))
    }
}

fn log_pending_scalar_conversion(entry: &PendingScalar) {
    let arg_summary = summarize_runtime_funcargs(&entry.arguments);
    let return_ty = describe_runtime_logicaltype(&entry.returns);
    let option_summary = summarize_funcopts(entry.options.as_ref());
    eprintln!(
        "[extension-manager] forwarding scalar '{extension}:{name}' (callback={callback}, args={arg_summary}, returns={return_ty}, opts={option_summary})",
        extension = entry.extension,
        name = entry.name,
        callback = entry.callback_handle,
    );
}

fn log_pending_batch_summary(data: &PendingRegistrationsData) {
    #[derive(Default)]
    struct Counters {
        scalars: usize,
        tables: usize,
        aggregates: usize,
    }
    let mut per_extension: BTreeMap<&str, Counters> = BTreeMap::new();
    for entry in &data.scalars {
        per_extension
            .entry(entry.extension.as_str())
            .or_default()
            .scalars += 1;
    }
    for entry in &data.tables {
        per_extension
            .entry(entry.extension.as_str())
            .or_default()
            .tables += 1;
    }
    for entry in &data.aggregates {
        per_extension
            .entry(entry.extension.as_str())
            .or_default()
            .aggregates += 1;
    }
    if per_extension.is_empty() {
        eprintln!("[extension-manager] pending registration batch empty; nothing to forward");
        return;
    }
    for (extension, counts) in per_extension {
        eprintln!(
            "[extension-manager] pending batch summary for '{extension}': scalars={}, tables={}, aggregates={}",
            counts.scalars, counts.tables, counts.aggregates
        );
    }
}

fn log_pending_table_conversion(entry: &PendingTable) {
    let arg_summary = summarize_runtime_funcargs(&entry.arguments);
    let column_summary = summarize_runtime_columns(&entry.columns);
    let option_summary = summarize_extopts(entry.options.as_ref());
    eprintln!(
        "[extension-manager] forwarding table '{extension}:{name}' (callback={callback}, args={arg_summary}, columns={column_summary}, opts={option_summary})",
        extension = entry.extension,
        name = entry.name,
        callback = entry.callback_handle,
    );
}

fn log_pending_aggregate_conversion(entry: &PendingAggregate) {
    let arg_summary = summarize_runtime_funcargs(&entry.arguments);
    let return_ty = describe_runtime_logicaltype(&entry.returns);
    let option_summary = summarize_funcopts(entry.options.as_ref());
    eprintln!(
        "[extension-manager] forwarding aggregate '{extension}:{name}' (callback={callback}, args={arg_summary}, returns={return_ty}, opts={option_summary})",
        extension = entry.extension,
        name = entry.name,
        callback = entry.callback_handle,
    );
}

fn convert_config_error(err: core_config_exports::Configerror) -> extension_types::Configerror {
    match err {
        core_config_exports::Configerror::Invalidkey(msg) => {
            extension_types::Configerror::Invalidkey(msg.into())
        }
        core_config_exports::Configerror::Typemismatch(msg) => {
            extension_types::Configerror::Typemismatch(msg.into())
        }
        core_config_exports::Configerror::Unavailable(msg) => {
            extension_types::Configerror::Unavailable(msg.into())
        }
        core_config_exports::Configerror::Internalconfig(msg) => {
            extension_types::Configerror::Internalconfig(msg.into())
        }
    }
}

fn map_config_trap(err: wasmtime::Error) -> extension_types::Configerror {
    extension_types::Configerror::Internalconfig(err.to_string())
}

fn convert_log_level_to_core(level: extension_logging::Loglevel) -> core_logging_exports::Loglevel {
    match level {
        extension_logging::Loglevel::Trace => core_logging_exports::Loglevel::Trace,
        extension_logging::Loglevel::Debug => core_logging_exports::Loglevel::Debug,
        extension_logging::Loglevel::Info => core_logging_exports::Loglevel::Info,
        extension_logging::Loglevel::Warn => core_logging_exports::Loglevel::Warn,
        extension_logging::Loglevel::Error => core_logging_exports::Loglevel::Error,
    }
}

fn convert_log_fields(
    fields: Vec<extension_logging::Logfield>,
) -> Vec<core_logging_exports::Logfield> {
    fields
        .into_iter()
        .map(|field| core_logging_exports::Logfield {
            key: field.key.into(),
            value: field.value.into(),
        })
        .collect()
}

fn trap_to_cli_string(err: wasmtime::Error) -> CliString {
    err.to_string().into()
}

fn core_err_to_cli(err: cli_types::Duckerror) -> cli_types::Duckerror {
    err
}

fn instantiate_core(
    engine: &Engine,
    component_path: &Path,
    wasi_ctx: WasiCtx,
    extension_manager: Arc<Mutex<ExtensionManager>>,
) -> Result<CoreExecution> {
    let component = Component::from_file(engine, component_path).with_context(|| {
        format!(
            "failed to load core component at {}",
            component_path.display()
        )
    })?;
    let mut linker = Linker::<CoreStoreState>::new(engine);
    p2::add_to_linker_sync(&mut linker)?;
    core_host_loader::add_to_linker::<CoreStoreState, CoreStoreState>(&mut linker, |state| state)?;
    core_extension_hooks::add_to_linker::<CoreStoreState, CoreStoreState>(&mut linker, |state| {
        state
    })?;
    core_callback_dispatch::add_to_linker::<CoreStoreState, CoreStoreState>(
        &mut linker,
        |state| state,
    )?;

    let mut store = Store::new(
        engine,
        CoreStoreState {
            table: ResourceTable::new(),
            wasi: wasi_ctx,
            extension_manager,
        },
    );

    let instance_pre = linker.instantiate_pre(&component)?;
    let pre = duckdb_core_bindings::LibduckdbPre::new(instance_pre)?;
    let bindings = pre.instantiate(store.as_context_mut())?;
    Ok(CoreExecution { store, bindings })
}

fn build_engine() -> Result<Engine> {
    let mut config = Config::new();
    config.wasm_component_model(true);
    Engine::new(&config).context("failed to create Wasmtime engine")
}

fn build_wasi_ctx_with_pipes(
    args: &[String],
    preopens: &[(&Path, &str)],
    stdin: MemoryInputPipe,
    stdout: MemoryOutputPipe,
    stderr: MemoryOutputPipe,
) -> Result<WasiCtx> {
    let mut builder = WasiCtxBuilder::new();
    builder.args(args);
    builder.stdin(stdin);
    builder.stdout(stdout);
    builder.stderr(stderr);
    builder.inherit_env();
    for (host, guest) in preopens {
        builder
            .preopened_dir(host, guest, DirPerms::all(), FilePerms::all())
            .with_context(|| {
                format!(
                    "failed to preopen directory {} as {}",
                    host.display(),
                    guest
                )
            })?;
    }
    Ok(builder.build())
}

fn build_wasi_ctx_inherit(args: &[String], preopens: &[(&Path, &str)]) -> Result<WasiCtx> {
    let mut builder = WasiCtxBuilder::new();
    builder.args(args);
    builder.inherit_env();
    builder.inherit_stdin();
    builder.inherit_stdout();
    builder.inherit_stderr();
    for (host, guest) in preopens {
        builder
            .preopened_dir(host, guest, DirPerms::all(), FilePerms::all())
            .with_context(|| {
                format!(
                    "failed to preopen directory {} as {}",
                    host.display(),
                    guest
                )
            })?;
    }
    Ok(builder.build())
}

fn workspace_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("tests directory")
        .parent()
        .expect("workspace root")
        .to_path_buf()
}

fn locate_component(filename: &str) -> Result<PathBuf> {
    let root = workspace_root().join("target/wasm32-wasip2");
    let candidates = [
        root.join("release").join(filename),
        root.join("debug").join(filename),
    ];
    for path in candidates {
        if path.exists() {
            return Ok(path);
        }
    }
    anyhow::bail!("component artifact {filename} not found in wasm32-wasip2 target directory")
}

#[derive(Clone, Debug)]
pub struct ComponentArtifacts {
    pub core_component: PathBuf,
    pub cli_component: PathBuf,
}

impl ComponentArtifacts {
    pub fn resolve_default() -> Result<Self> {
        Ok(Self {
            core_component: locate_component("duckdb_core_component.wasm")?,
            cli_component: locate_component("duckdb_cli_component.wasm")?,
        })
    }

    pub fn new(core_component: PathBuf, cli_component: PathBuf) -> Self {
        Self {
            core_component,
            cli_component,
        }
    }
}

static EXTENSION_ROOT: OnceLock<PathBuf> = OnceLock::new();

pub fn set_extension_root<P: Into<PathBuf>>(path: P) {
    let path = path.into();
    if EXTENSION_ROOT.set(path).is_err() {
        // already configured; ignore to avoid panic during tests configuring multiple times
    }
}

fn extension_artifact_path(name: &str) -> PathBuf {
    let root = EXTENSION_ROOT
        .get()
        .cloned()
        .unwrap_or_else(|| workspace_root().join("artifacts/extensions"));
    root.join(format!("{name}.wasm"))
}

fn sanitize_extension_name(raw: &str) -> String {
    let mut sanitized = String::with_capacity(raw.len());
    for ch in raw.chars() {
        if ch.is_ascii_alphanumeric() || ch == '_' || ch == '-' {
            sanitized.push(ch);
        } else {
            sanitized.push('_');
        }
    }
    if sanitized.is_empty() {
        sanitized.push('_');
    }
    sanitized
}

fn convert_extension_funcargs(
    args: Vec<extension_runtime::Funcarg>,
) -> Vec<core_runtime_exports::Funcarg> {
    args.into_iter()
        .map(|arg| core_runtime_exports::Funcarg {
            name: arg.name,
            logical: convert_extension_logicaltype(arg.logical),
        })
        .collect()
}

fn convert_extension_logicaltype(
    ty: extension_runtime::Logicaltype,
) -> core_runtime_exports::Logicaltype {
    match ty {
        extension_runtime::Logicaltype::Boolean => core_runtime_exports::Logicaltype::Boolean,
        extension_runtime::Logicaltype::Int64 => core_runtime_exports::Logicaltype::Int64,
        extension_runtime::Logicaltype::Uint64 => core_runtime_exports::Logicaltype::Uint64,
        extension_runtime::Logicaltype::Float64 => core_runtime_exports::Logicaltype::Float64,
        extension_runtime::Logicaltype::Text => core_runtime_exports::Logicaltype::Text,
        extension_runtime::Logicaltype::Blob => core_runtime_exports::Logicaltype::Blob,
    }
}

fn convert_extension_funcopts(opts: extension_runtime::Funcopts) -> core_runtime_exports::Funcopts {
    core_runtime_exports::Funcopts {
        description: opts.description,
        tags: opts.tags.into_iter().collect(),
        attributes: convert_extension_funcflags(opts.attributes),
    }
}

fn convert_extension_columndefs(
    columns: Vec<extension_runtime::Columndef>,
) -> Vec<core_runtime_exports::Columndef> {
    columns
        .into_iter()
        .map(|col| core_runtime_exports::Columndef {
            name: col.name,
            logical: convert_extension_logicaltype(col.logical),
        })
        .collect()
}

fn convert_extension_extopts(opts: extension_runtime::Extopts) -> core_runtime_exports::Extopts {
    core_runtime_exports::Extopts {
        description: opts.description,
        tags: opts.tags.into_iter().collect(),
    }
}

fn convert_extension_funcflags(flags: extension_types::Funcflags) -> core_types::Funcflags {
    let mut result = core_types::Funcflags::empty();
    if flags.contains(extension_types::Funcflags::DETERMINISTIC) {
        result |= core_types::Funcflags::DETERMINISTIC;
    }
    if flags.contains(extension_types::Funcflags::COMMUTATIVE) {
        result |= core_types::Funcflags::COMMUTATIVE;
    }
    if flags.contains(extension_types::Funcflags::STATELESS) {
        result |= core_types::Funcflags::STATELESS;
    }
    if flags.contains(extension_types::Funcflags::SIDEEFFECTING) {
        result |= core_types::Funcflags::SIDEEFFECTING;
    }
    if flags.contains(extension_types::Funcflags::DEPRECATED) {
        result |= core_types::Funcflags::DEPRECATED;
    }
    result
}

fn convert_core_duckvalue_to_extension(value: core_types::Duckvalue) -> extension_types::Duckvalue {
    match value {
        core_types::Duckvalue::Null => extension_types::Duckvalue::Null,
        core_types::Duckvalue::Boolean(v) => extension_types::Duckvalue::Boolean(v),
        core_types::Duckvalue::Int64(v) => extension_types::Duckvalue::Int64(v),
        core_types::Duckvalue::Uint64(v) => extension_types::Duckvalue::Uint64(v),
        core_types::Duckvalue::Float64(v) => extension_types::Duckvalue::Float64(v),
        core_types::Duckvalue::Text(v) => extension_types::Duckvalue::Text(v),
        core_types::Duckvalue::Blob(v) => extension_types::Duckvalue::Blob(v),
    }
}

fn convert_extension_duckvalue_to_core(value: extension_types::Duckvalue) -> core_types::Duckvalue {
    match value {
        extension_types::Duckvalue::Null => core_types::Duckvalue::Null,
        extension_types::Duckvalue::Boolean(v) => core_types::Duckvalue::Boolean(v),
        extension_types::Duckvalue::Int64(v) => core_types::Duckvalue::Int64(v),
        extension_types::Duckvalue::Uint64(v) => core_types::Duckvalue::Uint64(v),
        extension_types::Duckvalue::Float64(v) => core_types::Duckvalue::Float64(v),
        extension_types::Duckvalue::Text(v) => core_types::Duckvalue::Text(v),
        extension_types::Duckvalue::Blob(v) => core_types::Duckvalue::Blob(v),
    }
}

fn convert_core_invokeinfo(
    ctx: core_callback_dispatch::Invokeinfo,
) -> extension_runtime::Invokeinfo {
    extension_runtime::Invokeinfo {
        rowindex: ctx.rowindex,
        iswindow: ctx.iswindow,
    }
}

fn convert_extension_resultset_to_core(
    result: extension_runtime::Resultset,
) -> core_callback_dispatch::Resultset {
    result
        .into_iter()
        .map(|row| {
            row.into_iter()
                .map(convert_extension_duckvalue_to_core)
                .collect()
        })
        .collect()
}

fn convert_core_rowbatch_to_extension(
    batch: core_callback_dispatch::Rowbatch,
) -> extension_runtime::Rowbatch {
    batch
        .into_iter()
        .map(|row| {
            row.into_iter()
                .map(convert_core_duckvalue_to_extension)
                .collect()
        })
        .collect()
}

fn convert_extension_duckerror_to_core(err: extension_types::Duckerror) -> core_types::Duckerror {
    match err {
        extension_types::Duckerror::Invalidargument(v) => core_types::Duckerror::Invalidargument(v),
        extension_types::Duckerror::Unsupported(v) => core_types::Duckerror::Unsupported(v),
        extension_types::Duckerror::Invalidstate(v) => core_types::Duckerror::Invalidstate(v),
        extension_types::Duckerror::Io(v) => core_types::Duckerror::Io(v),
        extension_types::Duckerror::Internal(v) => core_types::Duckerror::Internal(v),
    }
}

fn convert_core_duckerror_to_extension(err: core_types::Duckerror) -> extension_types::Duckerror {
    match err {
        core_types::Duckerror::Invalidargument(v) => extension_types::Duckerror::Invalidargument(v),
        core_types::Duckerror::Unsupported(v) => extension_types::Duckerror::Unsupported(v),
        core_types::Duckerror::Invalidstate(v) => extension_types::Duckerror::Invalidstate(v),
        core_types::Duckerror::Io(v) => extension_types::Duckerror::Io(v),
        core_types::Duckerror::Internal(v) => extension_types::Duckerror::Internal(v),
    }
}

fn map_runtime_trap(err: wasmtime::Error) -> extension_types::Duckerror {
    extension_types::Duckerror::Internal(format!("core runtime trap: {err}"))
}

fn map_extension_trap(err: wasmtime::Error) -> extension_types::Duckerror {
    extension_types::Duckerror::Internal(format!("extension trap: {err}"))
}

pub struct CliHarness {
    store: Store<HostState>,
    cli: duckdb_cli_bindings::DuckdbCli,
    stdout: MemoryOutputPipe,
    stderr: MemoryOutputPipe,
}

impl CliHarness {
    pub fn new(args: &[impl AsRef<str>], preopens: &[(&Path, &str)]) -> Result<Self> {
        let artifacts = ComponentArtifacts::resolve_default()?;
        Self::with_artifacts(&artifacts, args, preopens)
    }

    pub fn with_artifacts(
        artifacts: &ComponentArtifacts,
        args: &[impl AsRef<str>],
        preopens: &[(&Path, &str)],
    ) -> Result<Self> {
        let engine = build_engine()?;
        let owned_preopens = resolve_preopens_with_default(preopens)?;
        let preopen_refs: Vec<(&Path, &str)> = owned_preopens
            .iter()
            .map(|(host, guest)| (host.as_path(), guest.as_str()))
            .collect();

        let args_vec: Vec<String> = args.iter().map(|s| s.as_ref().to_owned()).collect();
        let stdin = MemoryInputPipe::new("");
        let stdout = MemoryOutputPipe::new(64 * 1024);
        let stderr = MemoryOutputPipe::new(64 * 1024);
        let stdout_clone = stdout.clone();
        let stderr_clone = stderr.clone();

        let cli_wasi =
            build_wasi_ctx_with_pipes(&args_vec, &preopen_refs, stdin, stdout_clone, stderr_clone)?;
        let core_wasi = build_wasi_ctx_with_pipes(
            &[String::from("duckdb-core")],
            &preopen_refs,
            MemoryInputPipe::new(""),
            stdout.clone(),
            stderr.clone(),
        )?;

        let extension_manager = Arc::new(Mutex::new(ExtensionManager::new(engine.clone())));
        let core_exec = instantiate_core(
            &engine,
            &artifacts.core_component,
            core_wasi,
            extension_manager.clone(),
        )?;
        let core = Arc::new(Mutex::new(core_exec));
        {
            let mut manager = extension_manager
                .lock()
                .expect("extension manager mutex poisoned");
            manager.attach_core(core.clone());
        }
        let host_state = HostState {
            table: ResourceTable::new(),
            wasi: cli_wasi,
            core: core.clone(),
            extension_manager: extension_manager.clone(),
            next_resource_id: 1,
            connections: HashMap::new(),
            streams: HashMap::new(),
            pending_connection_drops: Vec::new(),
            pending_stream_drops: Vec::new(),
        };
        let mut store = Store::new(&engine, host_state);

        let mut linker = Linker::<HostState>::new(&engine);
        p2::add_to_linker_sync(&mut linker)?;
        cli_db::add_to_linker::<HostState, HostState>(&mut linker, |state| state)?;
        linker
            .instance("duckdb:component/host-extension-loader")?
            .func_wrap(
                "request-load",
                |mut store: StoreContextMut<'_, HostState>, (extension,): (String,)| {
                    store
                        .data_mut()
                        .request_extension_load(&extension)
                        .map(|handled| (handled,))
                },
            )?;

        let cli_component =
            Component::from_file(&engine, &artifacts.cli_component).with_context(|| {
                format!(
                    "failed to load CLI component from {}",
                    artifacts.cli_component.display()
                )
            })?;
        let instance_pre = linker.instantiate_pre(&cli_component)?;
        let cli_pre = duckdb_cli_bindings::DuckdbCliPre::new(instance_pre)?;
        let cli = cli_pre.instantiate(store.as_context_mut())?;

        Ok(Self {
            store,
            cli,
            stdout,
            stderr,
        })
    }

    pub fn preload_extension(&mut self, name: &str) -> Result<()> {
        self.store
            .data_mut()
            .preload_extension(name)
            .with_context(|| format!("failed to preload extension {name}"))?;
        Ok(())
    }

    pub fn run(&mut self) -> wasmtime::Result<Result<(), ()>> {
        let result = self
            .cli
            .wasi_cli_run()
            .call_run(self.store.as_context_mut());
        if let Ok(Ok(())) = result {
            if let Err(err) = self.store.data_mut().drain_pending_resource_drops() {
                return Err(wasmtime::Error::msg(format!(
                    "failed to finalize resource drops: {err:?}"
                )));
            }
        }
        result
    }

    pub fn stdout(&self) -> Result<String> {
        String::from_utf8(self.stdout.contents().to_vec())
            .context("stdout stream contained invalid UTF-8")
    }

    #[allow(dead_code)]
    pub fn stderr(&self) -> Result<String> {
        String::from_utf8(self.stderr.contents().to_vec())
            .context("stderr stream contained invalid UTF-8")
    }
}

pub fn run_cli_with_stdio(
    artifacts: &ComponentArtifacts,
    args: &[impl AsRef<str>],
    preopens: &[(&Path, &str)],
) -> Result<Result<(), ()>> {
    let engine = build_engine()?;
    let args_vec: Vec<String> = args.iter().map(|s| s.as_ref().to_owned()).collect();
    let cli_wasi = build_wasi_ctx_inherit(&args_vec, preopens)?;
    let core_wasi = build_wasi_ctx_inherit(&[String::from("duckdb-core")], preopens)?;

    let extension_manager = Arc::new(Mutex::new(ExtensionManager::new(engine.clone())));
    let core_exec = instantiate_core(
        &engine,
        &artifacts.core_component,
        core_wasi,
        extension_manager.clone(),
    )?;
    let core = Arc::new(Mutex::new(core_exec));
    {
        let mut manager = extension_manager
            .lock()
            .expect("extension manager mutex poisoned");
        manager.attach_core(core.clone());
    }
    let host_state = HostState {
        table: ResourceTable::new(),
        wasi: cli_wasi,
        core: core.clone(),
        extension_manager: extension_manager.clone(),
        next_resource_id: 1,
        connections: HashMap::new(),
        streams: HashMap::new(),
        pending_connection_drops: Vec::new(),
        pending_stream_drops: Vec::new(),
    };
    let mut store = Store::new(&engine, host_state);

    let mut linker = Linker::<HostState>::new(&engine);
    p2::add_to_linker_sync(&mut linker)?;
    cli_db::add_to_linker::<HostState, HostState>(&mut linker, |state| state)?;
    linker
        .instance("duckdb:component/host-extension-loader")?
        .func_wrap(
            "request-load",
            |mut store: StoreContextMut<'_, HostState>, (extension,): (String,)| {
                store
                    .data_mut()
                    .request_extension_load(&extension)
                    .map(|handled| (handled,))
            },
        )?;

    let cli_component =
        Component::from_file(&engine, &artifacts.cli_component).with_context(|| {
            format!(
                "failed to load CLI component from {}",
                artifacts.cli_component.display()
            )
        })?;
    let instance_pre = linker.instantiate_pre(&cli_component)?;
    let cli_pre = duckdb_cli_bindings::DuckdbCliPre::new(instance_pre)?;
    let cli = cli_pre.instantiate(store.as_context_mut())?;

    cli.wasi_cli_run().call_run(store.as_context_mut())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    /// True if any rendered table row contains `value` as a `|`-delimited cell,
    /// ignoring the column-width padding the CLI applies to each cell.
    fn has_cell(stdout: &str, value: &str) -> bool {
        stdout
            .lines()
            .any(|line| line.split('|').map(str::trim).any(|cell| cell == value))
    }

    #[ignore]
    #[test]
    fn smoke_runs_sql_against_disk_database() -> Result<()> {
        let tempdir = tempdir().context("failed to create temporary directory")?;
        let db_host_path = tempdir.path().join("smoke.db");
        let db_guest_path = ":memory:";

        let command = "CREATE TABLE items(v INTEGER); \
                       INSERT INTO items VALUES (1), (2), (3); \
                       SELECT SUM(v) AS total, COUNT(*) AS count FROM items;";

        let args = ["duckdb-cli", db_guest_path, "-c", command];

        let preopens = [(tempdir.path(), ".")];

        let mut harness = CliHarness::new(&args, &preopens)?;
        let status = harness.run()?;
        if status.is_err() {
            let stdout_dump = harness.stdout().unwrap_or_default();
            let stderr_dump = harness.stderr().unwrap_or_default();
            panic!(
                "CLI returned error status
stdout:
{stdout_dump}
stderr:
{stderr_dump}"
            );
        }

        let stdout = harness.stdout()?;
        assert!(
            stdout.contains("| total | count |"),
            "expected aggregated header in stdout, got:\n{stdout}"
        );
        assert!(
            stdout.contains("| 6     | 3     |"),
            "expected aggregated row in stdout, got:\n{stdout}"
        );
        let _ = db_host_path;

        Ok(())
    }

    #[test]
    fn cli_loads_component_extension_via_duckdb_loader() -> Result<()> {
        ensure_sample_extension_artifact()?;

        let args = [
            "duckdb-cli",
            ":memory:",
            "--load-extension",
            "sample_extension",
            "-c",
            "select 42 as answer;",
        ];

        let mut harness = CliHarness::new(&args, &[])?;
        let status = harness.run()?;
        assert!(status.is_ok(), "CLI reported failure loading extension");

        let stdout = harness.stdout()?;
        assert!(
            has_cell(&stdout, "answer") && has_cell(&stdout, "42"),
            "expected query result in stdout after extension load, got:\n{}",
            stdout
        );

        Ok(())
    }

    #[test]
    fn cli_executes_sample_scalar_callback() -> Result<()> {
        ensure_sample_extension_artifact()?;

        let args = [
            "duckdb-cli",
            ":memory:",
            "--load-extension",
            "sample_extension",
            "-c",
            "select sample_plus_one(41) as answer;",
        ];

        let mut harness = CliHarness::new(&args, &[])?;
        let status = harness.run()?;
        assert!(
            status.is_ok(),
            "CLI reported failure invoking sample_plus_one: {:?}",
            harness.stderr().ok()
        );

        let stdout = harness.stdout()?;
        assert!(
            has_cell(&stdout, "answer") && has_cell(&stdout, "42"),
            "expected scalar callback output, got:\n{}",
            stdout
        );

        Ok(())
    }

    #[test]
    fn cli_executes_sample_table_function() -> Result<()> {
        ensure_sample_extension_artifact()?;

        let args = [
            "duckdb-cli",
            ":memory:",
            "--load-extension",
            "sample_extension",
            "-c",
            "select * from sample_emit_sequence(4);",
        ];

        let mut harness = CliHarness::new(&args, &[])?;
        let status = harness.run()?;
        assert!(
            status.is_ok(),
            "CLI reported failure invoking sample_emit_sequence: {:?}",
            harness.stderr().ok()
        );

        let stdout = harness.stdout()?;
        assert!(
            has_cell(&stdout, "value") && has_cell(&stdout, "3"),
            "expected table callback output, got:\n{}",
            stdout
        );

        Ok(())
    }

    #[test]
    fn cli_executes_sample_aggregate_function() -> Result<()> {
        ensure_sample_extension_artifact()?;

        let args = [
            "duckdb-cli",
            ":memory:",
            "--load-extension",
            "sample_extension",
            "-c",
            "select sample_sum(v) as total from (values (1),(2),(3),(4)) as t(v);",
        ];

        let mut harness = CliHarness::new(&args, &[])?;
        let status = harness.run()?;
        assert!(
            status.is_ok(),
            "CLI reported failure invoking sample_sum: {:?}",
            harness.stderr().ok()
        );

        let stdout = harness.stdout()?;
        assert!(
            has_cell(&stdout, "total") && has_cell(&stdout, "10"),
            "expected aggregate callback output, got:\n{}",
            stdout
        );

        Ok(())
    }

    #[test]
    fn load_sample_extension_component() -> Result<()> {
        let artifact = ensure_sample_extension_artifact()?;
        let engine = build_engine()?;
        let mut linker = Linker::<TestExtensionHost>::new(&engine);
        p2::add_to_linker_sync(&mut linker)?;
        extension_types::add_to_linker::<TestExtensionHost, TestExtensionHost>(
            &mut linker,
            |state| state,
        )?;
        extension_runtime::add_to_linker::<TestExtensionHost, TestExtensionHost>(
            &mut linker,
            |state| state,
        )?;
        extension_config::add_to_linker::<TestExtensionHost, TestExtensionHost>(
            &mut linker,
            |state| state,
        )?;
        extension_logging::add_to_linker::<TestExtensionHost, TestExtensionHost>(
            &mut linker,
            |state| state,
        )?;

        let component = Component::from_file(&engine, &artifact)?;
        let instance_pre = linker.instantiate_pre(&component)?;
        let pre = DuckdbExtensionPre::new(instance_pre)?;
        let mut store = Store::new(&engine, TestExtensionHost::new());
        let bindings = pre.instantiate(store.as_context_mut())?;
        let result = bindings
            .duckdb_extension_guest()
            .call_load(store.as_context_mut())
            .map_err(|err| anyhow::anyhow!(err))?;
        let load_result =
            result.map_err(|err| anyhow::anyhow!("duckdb extension returned error: {err:?}"))?;
        assert_eq!(load_result.name, "sample_extension");
        assert!(load_result.version.is_some());

        Ok(())
    }

    fn ensure_sample_extension_artifact() -> Result<PathBuf> {
        let workspace = workspace_root();
        let target_artifact =
            workspace.join("target/wasm32-wasip1/release/sample_extension_component.wasm");
        if !target_artifact.exists() {
            let prebuilt = workspace.join("artifacts/extensions/sample_extension.wasm");
            if prebuilt.exists() {
                if let Some(parent) = target_artifact.parent() {
                    fs::create_dir_all(parent)
                        .with_context(|| format!("failed to create {}", parent.display()))?;
                }
                fs::copy(&prebuilt, &target_artifact).with_context(|| {
                    format!(
                        "failed to copy prebuilt sample extension from {} to {}",
                        prebuilt.display(),
                        target_artifact.display()
                    )
                })?;
            } else {
                build_sample_extension(&workspace)?;
            }
        }
        let extensions_dir = workspace.join("artifacts/extensions");
        fs::create_dir_all(&extensions_dir)
            .with_context(|| format!("failed to create {}", extensions_dir.display()))?;
        let dest = extensions_dir.join("sample_extension.wasm");
        fs::copy(&target_artifact, &dest).with_context(|| {
            format!(
                "failed to copy sample extension from {} to {}",
                target_artifact.display(),
                dest.display()
            )
        })?;
        Ok(dest)
    }

    fn build_sample_extension(workspace: &Path) -> Result<()> {
        let status = Command::new("cargo")
            .args([
                "component",
                "build",
                "-p",
                "sample-extension-component",
                "--release",
                "--target",
                "wasm32-wasip1",
            ])
            .current_dir(workspace)
            .status()
            .context("failed to spawn cargo component build for sample extension")?;
        if !status.success() {
            anyhow::bail!("building sample extension component failed with status {status}");
        }
        Ok(())
    }

    struct TestExtensionHost {
        table: ResourceTable,
        wasi: WasiCtx,
        next_resource_id: u32,
    }

    impl TestExtensionHost {
        fn new() -> Self {
            let wasi = WasiCtxBuilder::new().inherit_env().inherit_stdio().build();
            Self {
                table: ResourceTable::new(),
                wasi,
                next_resource_id: 1,
            }
        }

        fn alloc_resource_id(&mut self) -> u32 {
            let id = self.next_resource_id;
            self.next_resource_id = self.next_resource_id.wrapping_add(1).max(1);
            id
        }
    }

    impl WasiView for TestExtensionHost {
        fn ctx(&mut self) -> WasiCtxView<'_> {
            WasiCtxView {
                ctx: &mut self.wasi,
                table: &mut self.table,
            }
        }
    }

    impl wasmtime::component::HasData for TestExtensionHost {
        type Data<'a> = &'a mut TestExtensionHost;
    }

    impl extension_types::Host for TestExtensionHost {}

    impl extension_runtime::Host for TestExtensionHost {
        fn get_capability(
            &mut self,
            kind: extension_runtime::Capabilitykind,
        ) -> Option<extension_runtime::Capability> {
            match kind {
                extension_runtime::Capabilitykind::Scalar => {
                    Some(extension_runtime::Capability::Scalar(
                        wasmtime::component::Resource::new_own(self.alloc_resource_id()),
                    ))
                }
                extension_runtime::Capabilitykind::Table => {
                    Some(extension_runtime::Capability::Table(
                        wasmtime::component::Resource::new_own(self.alloc_resource_id()),
                    ))
                }
                extension_runtime::Capabilitykind::Aggregate => {
                    Some(extension_runtime::Capability::Aggregate(
                        wasmtime::component::Resource::new_own(self.alloc_resource_id()),
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

    impl extension_runtime::HostScalarCallback for TestExtensionHost {
        fn new(
            &mut self,
            _handle: u32,
        ) -> wasmtime::component::Resource<extension_runtime::ScalarCallback> {
            wasmtime::component::Resource::new_own(self.alloc_resource_id())
        }

        fn call(
            &mut self,
            _self_: wasmtime::component::Resource<extension_runtime::ScalarCallback>,
            _args: BindgenVec<extension_types::Duckvalue>,
            _ctx: extension_runtime::Invokeinfo,
        ) -> Result<extension_types::Duckvalue, extension_types::Duckerror> {
            Err(unsupported_runtime_error())
        }

        fn drop(
            &mut self,
            _rep: wasmtime::component::Resource<extension_runtime::ScalarCallback>,
        ) -> wasmtime::Result<()> {
            Ok(())
        }
    }

    impl extension_runtime::HostTableCallback for TestExtensionHost {
        fn new(
            &mut self,
            _handle: u32,
        ) -> wasmtime::component::Resource<extension_runtime::TableCallback> {
            wasmtime::component::Resource::new_own(self.alloc_resource_id())
        }

        fn call(
            &mut self,
            _self_: wasmtime::component::Resource<extension_runtime::TableCallback>,
            _args: BindgenVec<extension_types::Duckvalue>,
        ) -> Result<extension_runtime::Resultset, extension_types::Duckerror> {
            Err(unsupported_runtime_error())
        }

        fn drop(
            &mut self,
            _rep: wasmtime::component::Resource<extension_runtime::TableCallback>,
        ) -> wasmtime::Result<()> {
            Ok(())
        }
    }

    impl extension_runtime::HostAggregateCallback for TestExtensionHost {
        fn new(
            &mut self,
            _handle: u32,
        ) -> wasmtime::component::Resource<extension_runtime::AggregateCallback> {
            wasmtime::component::Resource::new_own(self.alloc_resource_id())
        }

        fn call(
            &mut self,
            _self_: wasmtime::component::Resource<extension_runtime::AggregateCallback>,
            _rows: extension_runtime::Rowbatch,
        ) -> Result<extension_types::Duckvalue, extension_types::Duckerror> {
            Err(unsupported_runtime_error())
        }

        fn drop(
            &mut self,
            _rep: wasmtime::component::Resource<extension_runtime::AggregateCallback>,
        ) -> wasmtime::Result<()> {
            Ok(())
        }
    }

    impl extension_runtime::HostPragmaCallback for TestExtensionHost {
        fn new(
            &mut self,
            _handle: u32,
        ) -> wasmtime::component::Resource<extension_runtime::PragmaCallback> {
            wasmtime::component::Resource::new_own(self.alloc_resource_id())
        }

        fn call(
            &mut self,
            _self_: wasmtime::component::Resource<extension_runtime::PragmaCallback>,
            _args: BindgenVec<extension_types::Duckvalue>,
        ) -> Result<Option<extension_types::Duckvalue>, extension_types::Duckerror> {
            Err(unsupported_runtime_error())
        }

        fn drop(
            &mut self,
            _rep: wasmtime::component::Resource<extension_runtime::PragmaCallback>,
        ) -> wasmtime::Result<()> {
            Ok(())
        }
    }

    impl extension_runtime::HostScalarRegistry for TestExtensionHost {
        fn register(
            &mut self,
            _self_: wasmtime::component::Resource<extension_runtime::ScalarRegistry>,
            _name: String,
            _arguments: BindgenVec<extension_runtime::Funcarg>,
            _returns: extension_runtime::Logicaltype,
            _callback: wasmtime::component::Resource<extension_runtime::ScalarCallback>,
            _options: Option<extension_runtime::Funcopts>,
        ) -> Result<u32, extension_types::Duckerror> {
            Ok(self.alloc_resource_id())
        }

        fn drop(
            &mut self,
            _rep: wasmtime::component::Resource<extension_runtime::ScalarRegistry>,
        ) -> wasmtime::Result<()> {
            Ok(())
        }
    }

    impl extension_runtime::HostTableRegistry for TestExtensionHost {
        fn register(
            &mut self,
            _self_: wasmtime::component::Resource<extension_runtime::TableRegistry>,
            _name: String,
            _arguments: BindgenVec<extension_runtime::Funcarg>,
            _columns: BindgenVec<extension_runtime::Columndef>,
            _callback: wasmtime::component::Resource<extension_runtime::TableCallback>,
            _options: Option<extension_runtime::Extopts>,
        ) -> Result<u32, extension_types::Duckerror> {
            Ok(self.alloc_resource_id())
        }

        fn drop(
            &mut self,
            _rep: wasmtime::component::Resource<extension_runtime::TableRegistry>,
        ) -> wasmtime::Result<()> {
            Ok(())
        }
    }

    impl extension_runtime::HostAggregateRegistry for TestExtensionHost {
        fn register(
            &mut self,
            _self_: wasmtime::component::Resource<extension_runtime::AggregateRegistry>,
            _name: String,
            _arguments: BindgenVec<extension_runtime::Funcarg>,
            _returns: extension_runtime::Logicaltype,
            _callback: wasmtime::component::Resource<extension_runtime::AggregateCallback>,
            _options: Option<extension_runtime::Funcopts>,
        ) -> Result<u32, extension_types::Duckerror> {
            Ok(self.alloc_resource_id())
        }

        fn drop(
            &mut self,
            _rep: wasmtime::component::Resource<extension_runtime::AggregateRegistry>,
        ) -> wasmtime::Result<()> {
            Ok(())
        }
    }

    impl extension_runtime::HostPragmaRegistry for TestExtensionHost {
        fn register_call(
            &mut self,
            _self_: wasmtime::component::Resource<extension_runtime::PragmaRegistry>,
            _name: String,
            _arguments: BindgenVec<extension_runtime::Funcarg>,
            _returns: extension_runtime::Logicaltype,
            _callback: wasmtime::component::Resource<extension_runtime::PragmaCallback>,
            _options: Option<extension_runtime::Extopts>,
        ) -> Result<u32, extension_types::Duckerror> {
            Err(unsupported_runtime_error())
        }

        fn drop(
            &mut self,
            _rep: wasmtime::component::Resource<extension_runtime::PragmaRegistry>,
        ) -> wasmtime::Result<()> {
            Ok(())
        }
    }

    impl extension_runtime::HostMacroRegistry for TestExtensionHost {
        fn register_scalar(
            &mut self,
            _self_: wasmtime::component::Resource<extension_runtime::MacroRegistry>,
            _name: String,
            _parameters: BindgenVec<String>,
            _body_sql: String,
            _options: Option<extension_runtime::Extopts>,
        ) -> Result<bool, extension_types::Duckerror> {
            Err(unsupported_runtime_error())
        }

        fn drop(
            &mut self,
            _rep: wasmtime::component::Resource<extension_runtime::MacroRegistry>,
        ) -> wasmtime::Result<()> {
            Ok(())
        }
    }

    impl extension_config::Host for TestExtensionHost {
        fn provider_version(&mut self) -> String {
            "test-extension-host".into()
        }

        fn list_keys(&mut self, _prefix: Option<String>) -> BindgenVec<String> {
            Vec::new().into()
        }

        fn get_string(
            &mut self,
            _path: String,
        ) -> Result<Option<String>, extension_types::Configerror> {
            Ok(None)
        }

        fn get_bool(
            &mut self,
            _path: String,
        ) -> Result<Option<bool>, extension_types::Configerror> {
            Ok(None)
        }

        fn get_i64(&mut self, _path: String) -> Result<Option<i64>, extension_types::Configerror> {
            Ok(None)
        }

        fn get_u64(&mut self, _path: String) -> Result<Option<u64>, extension_types::Configerror> {
            Ok(None)
        }

        fn get_f64(&mut self, _path: String) -> Result<Option<f64>, extension_types::Configerror> {
            Ok(None)
        }

        fn get_bytes(
            &mut self,
            _path: String,
        ) -> Result<Option<BindgenVec<u8>>, extension_types::Configerror> {
            Ok(None)
        }

        fn get_string_list(
            &mut self,
            _path: String,
        ) -> Result<Option<BindgenVec<String>>, extension_types::Configerror> {
            Ok(None)
        }
    }

    impl extension_logging::Host for TestExtensionHost {
        fn log(
            &mut self,
            _level: extension_logging::Loglevel,
            _message: String,
            _target: Option<String>,
        ) {
        }

        fn log_fields(
            &mut self,
            _level: extension_logging::Loglevel,
            _message: String,
            _fields: BindgenVec<extension_logging::Logfield>,
        ) {
        }
    }
}
pub mod duckdb_extension_bindings {
    wasmtime::component::bindgen!({
        path: "./wit",
        world: "duckdb:extension-host/duckdb-extension",
        require_store_data_send: true,
    });
}
fn resolve_preopens_with_default(preopens: &[(&Path, &str)]) -> Result<Vec<(PathBuf, String)>> {
    let mut merged = Vec::with_capacity(preopens.len() + 1);
    merged.push((std::env::current_dir()?, ".".to_string()));
    for (host, guest) in preopens {
        merged.push((host.to_path_buf(), guest.to_string()));
    }
    Ok(merged)
}
