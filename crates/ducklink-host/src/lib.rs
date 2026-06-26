pub mod duckdb_core_bindings {
    wasmtime::component::bindgen!({
        path: "../../../duckdb-wasm/core/wit",
        world: "duckdb:component/libduckdb",
        with: {
            "wasi:cli/environment": wasmtime_wasi::p2::bindings::cli::environment,
            "wasi:cli/stdout": wasmtime_wasi::p2::bindings::cli::stdout,
            "wasi:cli/stderr": wasmtime_wasi::p2::bindings::cli::stderr,
            "wasi:filesystem/preopens": wasmtime_wasi::p2::bindings::filesystem::preopens,
            "wasi:filesystem/types": wasmtime_wasi::p2::bindings::filesystem::types,
            "wasi:io/streams": wasmtime_wasi::p2::bindings::io::streams,
        },
        require_store_data_send: true,
    });
}

pub mod duckdb_cli_bindings {
    wasmtime::component::bindgen!({
        path: "../../crates/ducklink-cli/wit",
        world: "duckdb:cli/duckdb-cli",
        with: {
            "wasi:cli/environment": wasmtime_wasi::p2::bindings::cli::environment,
            "wasi:cli/stdin": wasmtime_wasi::p2::bindings::cli::stdin,
            "wasi:cli/stdout": wasmtime_wasi::p2::bindings::cli::stdout,
            "wasi:cli/stderr": wasmtime_wasi::p2::bindings::cli::stderr,
            "wasi:filesystem/preopens": wasmtime_wasi::p2::bindings::filesystem::preopens,
            "wasi:filesystem/types": wasmtime_wasi::p2::bindings::filesystem::types,
        },
        require_store_data_send: true,
    });
}

pub mod dotcmd_bindings {
    wasmtime::component::bindgen!({
        path: "../../wit/dotcmd",
        world: "duckdb:dotcmd/dotcmd",
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
use duckdb_core_bindings::duckdb::extension::storage_host as core_storage_host;
use duckdb_core_bindings::duckdb::extension::index_host as core_index_host;
use duckdb_core_bindings::duckdb::extension::collation_host as core_collation_host;
use duckdb_core_bindings::duckdb::extension::pragma_host as core_pragma_host;
use duckdb_core_bindings::duckdb::extension::files_host as core_files_host;
use duckdb_core_bindings::duckdb::extension::types as core_types;
use duckdb_core_bindings::tvm::memory::bytes as core_tvm_bytes;
use duckdb_core_bindings::tvm::memory::manager as core_tvm_manager;
use duckdb_core_bindings::tvm::memory::types as core_tvm_types;
use duckdb_core_bindings::exports::duckdb::component::database as core_db_exports;
use duckdb_core_bindings::exports::duckdb::extension::{
    config as core_config_exports, logging as core_logging_exports, runtime as core_runtime_exports,
};
use ducklink_runtime::duckdb_extension_bindings::duckdb::extension::{
    runtime as extension_runtime, types as extension_types,
};
// The catalog/config/files/logging interfaces + DuckdbExtensionPre are only
// named by the in-crate test harness now (TestExtensionHost mocks + a direct
// instantiate); the engine itself moved to ducklink-runtime.
#[cfg(test)]
use ducklink_runtime::duckdb_extension_bindings::duckdb::extension::{
    catalog as extension_catalog, config as extension_config, files as extension_files,
    logging as extension_logging,
};
#[cfg(test)]
use ducklink_runtime::duckdb_extension_bindings::DuckdbExtensionPre;
use wasmtime::component::__internal::Vec as BindgenVec;
use ducklink_runtime::{CallbackEntry, CallbackKind, CallbackRegistry};
// M2b: the storage interface's scan types (scan-request / scan-filter /
// compare-op) used to drive a pushdown scan into a storage component.
use ducklink_runtime::extension::storage_scan;
// The extension engine (store-state, loaded-component instance, capture model)
// now lives in ducklink-runtime; the host supplies the Direction-1 service sink
// (CoreServices) and the Direction-1 registration sink (convert_pending_*).
use ducklink_runtime::{
    describe_runtime_logicaltype, summarize_extopts, summarize_funcopts,
    summarize_registration_names, summarize_runtime_columns, summarize_runtime_funcargs,
    ConfigError, ExtensionInstance, ExtensionServices, LogField, LogLevel,
    PendingRegistrationsData,
};
use wasmtime::component::{Component, Linker, Resource, ResourceAny, ResourceTable};
use wasmtime::{AsContextMut, Config, Engine, Store, StoreContextMut};

/// The `compose:dynlink/linker` host implementation now lives in
/// `ducklink-runtime` (so the extension load path can wire it); re-exported
/// here under the original path so the dotcmd path + tests are unchanged.
use ducklink_runtime::compose_dynlink;
pub use ducklink_runtime::compose_dynlink::{ProviderPreopen, ProviderRegistry};
/// Test/embedder support surface for the `compose:dynlink/linker` host
/// import: the `DynState` store state, the `imports_linker` gate, and the
/// `add_to_linker` wiring. Used by the integration test that drives the
/// framework's dlopen guest through ducklink-host's wasmtime.
pub mod compose_dynlink_test_support {
    pub use ducklink_runtime::compose_dynlink::{add_to_linker, imports_linker, DynState};
}
mod delta_rewrite;
mod prefix;
mod ui_server;
pub use ui_server::{serve_ui, UiMode};
mod handler;
pub use handler::HandlerRegistry;
mod httpd;
pub use httpd::{serve_httpd, HttpdOptions, TlsMode};
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
    // Tiered Virtual Memory: host-owned regions back DuckDB's >4 GiB spill tier.
    tvm: tvm_core::RegionDirectory<tvm_core::VecBackedRegion>,
    // Per-slot generation layer over tvm_core (whose handle generation is
    // region-level). Keyed by (region-id, offset): a per-slot generation that
    // bumps on each reallocation, plus the live tvm_core handle (None once
    // freed). The WIT handle carries the slot generation, so a stale handle to a
    // freed or freed-then-reused slot is rejected instead of hitting the block
    // that reused the slot. See web/tvm-host.mjs for the browser-host mirror.
    tvm_slots: std::collections::HashMap<(u16, u32), (u16, Option<tvm_core::Handle>)>,
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

    fn call_scalar_batch(
        &mut self,
        handle: u32,
        rows: core_callback_dispatch::Rowbatch,
        ctx: core_callback_dispatch::Invokeinfo,
    ) -> Result<Vec<core_types::Duckvalue>, core_types::Duckerror> {
        // The whole chunk crosses to the extension in ONE call; the extension
        // loops internally. This removes the per-row host->extension wasmtime
        // invocation overhead (the dominant share). Row i's index is
        // ctx.rowindex + i, derived extension-side.
        let converted_rows: Vec<Vec<_>> = rows
            .into_iter()
            .map(|row| {
                row.into_iter()
                    .map(convert_core_duckvalue_to_extension)
                    .collect()
            })
            .collect();
        let converted_ctx = convert_core_invokeinfo(ctx);
        let mut manager = self
            .extension_manager
            .lock()
            .expect("extension manager mutex poisoned");
        manager
            .dispatch_scalar_batch(handle, &converted_rows, converted_ctx)
            .map(|vals| {
                vals.into_iter()
                    .map(convert_extension_duckvalue_to_core)
                    .collect()
            })
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

    fn call_cast(
        &mut self,
        handle: u32,
        value: core_types::Duckvalue,
    ) -> Result<core_types::Duckvalue, core_types::Duckerror> {
        let converted = convert_core_duckvalue_to_extension(value);
        let mut manager = self
            .extension_manager
            .lock()
            .expect("extension manager mutex poisoned");
        manager
            .dispatch_cast(handle, &converted)
            .map(convert_extension_duckvalue_to_core)
            .map_err(convert_extension_duckerror_to_core)
    }
}

// M2a: the core imports `duckdb:extension/storage-host` for read-only
// foreign-catalog enumeration; the host PROVIDES it and routes each call through
// the ExtensionManager to the backing storage component's `storage-dispatch`
// export (mirroring callback-dispatch above). storage-attach reads the host file
// named by the DSN and stages it into the component.
impl core_storage_host::Host for CoreStoreState {
    fn storage_list_types(&mut self) -> Vec<String> {
        let manager = self
            .extension_manager
            .lock()
            .expect("extension manager mutex poisoned");
        manager.registered_storage_types()
    }

    fn storage_attach(&mut self, dsn: String) -> Result<u32, core_types::Duckerror> {
        let mut manager = self
            .extension_manager
            .lock()
            .expect("extension manager mutex poisoned");
        manager
            .dispatch_storage_attach(&dsn)
            .map_err(convert_extension_duckerror_to_core)
    }

    fn storage_list_tables(&mut self, catalog: u32) -> Result<Vec<String>, core_types::Duckerror> {
        let mut manager = self
            .extension_manager
            .lock()
            .expect("extension manager mutex poisoned");
        manager
            .dispatch_storage_list_tables(catalog)
            .map(|tables| tables.into_iter().map(Into::into).collect())
            .map_err(convert_extension_duckerror_to_core)
    }

    fn storage_table_columns(
        &mut self,
        catalog: u32,
        table: String,
    ) -> Result<Vec<core_types::Columndef>, core_types::Duckerror> {
        let mut manager = self
            .extension_manager
            .lock()
            .expect("extension manager mutex poisoned");
        manager
            .dispatch_storage_table_columns(catalog, &table)
            .map(|cols| {
                cols.into_iter()
                    .map(convert_extension_columndef_to_core)
                    .collect()
            })
            .map_err(convert_extension_duckerror_to_core)
    }

    // M2b scan surface: engine-driven projection + filter pushdown. The core
    // sends a scan-request (table + projection + filters + limit); the host
    // routes it to the backing component's storage-dispatch.
    fn storage_scan_open(
        &mut self,
        catalog: u32,
        request: core_storage_host::ScanRequest,
    ) -> Result<u32, core_types::Duckerror> {
        // Criterion 2: prove the pushdown reached the host with the projection +
        // filters the engine pushed.
        let filter_log: Vec<String> = request
            .filters
            .iter()
            .map(|f| {
                format!(
                    "(col {} {:?} {})",
                    f.column,
                    f.op,
                    describe_core_duckvalue(&f.value)
                )
            })
            .collect();
        eprintln!(
            "[storage-scan] dispatch_storage_scan_open catalog={} table={:?} projection={:?} filters=[{}] limit={:?}",
            catalog,
            request.table,
            request.projection,
            filter_log.join(", "),
            request.limit,
        );

        let scan_request = convert_core_scan_request_to_storage(request);
        let mut manager = self
            .extension_manager
            .lock()
            .expect("extension manager mutex poisoned");
        manager
            .dispatch_storage_scan_open(catalog, scan_request)
            .map_err(convert_extension_duckerror_to_core)
    }

    fn storage_scan_next(
        &mut self,
        scan: u32,
        max_rows: u32,
    ) -> Result<core_storage_host::Resultset, core_types::Duckerror> {
        let mut manager = self
            .extension_manager
            .lock()
            .expect("extension manager mutex poisoned");
        manager
            .dispatch_storage_scan_next(scan, max_rows)
            .map(convert_extension_resultset_to_core)
            .map_err(convert_extension_duckerror_to_core)
    }

    fn storage_scan_close(&mut self, scan: u32) -> Result<bool, core_types::Duckerror> {
        let mut manager = self
            .extension_manager
            .lock()
            .expect("extension manager mutex poisoned");
        manager
            .dispatch_storage_scan_close(scan)
            .map_err(convert_extension_duckerror_to_core)
    }
}

// Item 3 / M2a: the core imports `duckdb:extension/index-host` for custom-index
// build + search. The host PROVIDES it and routes each call through the
// ExtensionManager to the backing index component's `index-dispatch` export
// (mirroring storage-host). index-type-list lets the core register a wasm
// IndexType per declared type so `CREATE INDEX ... USING <type>` dispatches here.
impl core_index_host::Host for CoreStoreState {
    fn index_type_list(&mut self) -> Vec<String> {
        let manager = self
            .extension_manager
            .lock()
            .expect("extension manager mutex poisoned");
        manager.registered_index_types()
    }

    fn index_create(
        &mut self,
        type_name: String,
        index_name: String,
        dims: u32,
    ) -> Result<u32, core_types::Duckerror> {
        let mut manager = self
            .extension_manager
            .lock()
            .expect("extension manager mutex poisoned");
        manager
            .dispatch_index_create(&type_name, &index_name, dims)
            .map_err(convert_extension_duckerror_to_core)
    }

    fn index_append(
        &mut self,
        handle: u32,
        rowids: Vec<i64>,
        vectors: Vec<Vec<f32>>,
    ) -> Result<(), core_types::Duckerror> {
        let mut manager = self
            .extension_manager
            .lock()
            .expect("extension manager mutex poisoned");
        manager
            .dispatch_index_append(handle, &rowids, &vectors)
            .map_err(convert_extension_duckerror_to_core)
    }

    fn index_build(&mut self, handle: u32) -> Result<(), core_types::Duckerror> {
        let mut manager = self
            .extension_manager
            .lock()
            .expect("extension manager mutex poisoned");
        manager
            .dispatch_index_build(handle)
            .map_err(convert_extension_duckerror_to_core)
    }

    fn index_search(
        &mut self,
        handle: u32,
        query: Vec<f32>,
        k: u32,
    ) -> Result<Vec<core_index_host::IndexHit>, core_types::Duckerror> {
        let mut manager = self
            .extension_manager
            .lock()
            .expect("extension manager mutex poisoned");
        manager
            .dispatch_index_search(handle, &query, k)
            .map(|hits| {
                hits.into_iter()
                    .map(|h| core_index_host::IndexHit {
                        rowid: h.rowid,
                        distance: h.distance,
                    })
                    .collect()
            })
            .map_err(convert_extension_duckerror_to_core)
    }

    fn index_drop(&mut self, handle: u32) -> Result<(), core_types::Duckerror> {
        let mut manager = self
            .extension_manager
            .lock()
            .expect("extension manager mutex poisoned");
        manager
            .dispatch_index_drop(handle)
            .map_err(convert_extension_duckerror_to_core)
    }
}

// Item 2: the core imports `duckdb:extension/collation-host` to pull the
// collations components have declared. The host PROVIDES it, returning each
// collation's name + transform scalar + combinable flag. The core wraps each as
// a DuckDB collation (CreateCollationInfo) reusing the named, already-registered
// sort-key scalar -- no per-row dispatch (the scalar's own callback path drives
// the transform).
impl core_collation_host::Host for CoreStoreState {
    fn collation_list(&mut self) -> Vec<core_collation_host::CollationSpec> {
        let manager = self
            .extension_manager
            .lock()
            .expect("extension manager mutex poisoned");
        manager
            .registered_collations()
            .into_iter()
            .map(|(name, transform_scalar, combinable)| core_collation_host::CollationSpec {
                name,
                transform_scalar,
                combinable,
            })
            .collect()
    }
}

// Item 4: the core imports `duckdb:extension/pragma-host` to pull the pragmas
// components have declared. The host PROVIDES it, returning each pragma's name +
// callback handle. The core intercepts `PRAGMA <name>(...)`, dispatches via
// callback-dispatch.call-pragma (the component returns a SQL script), and runs
// that script -- no mid-callback re-entry into SQL.
impl core_pragma_host::Host for CoreStoreState {
    fn pragma_list(&mut self) -> Vec<core_pragma_host::PragmaSpec> {
        let manager = self
            .extension_manager
            .lock()
            .expect("extension manager mutex poisoned");
        manager
            .registered_pragmas()
            .into_iter()
            .map(|(name, callback_handle)| core_pragma_host::PragmaSpec {
                name,
                callback_handle,
            })
            .collect()
    }
}

// httpfs M2: the core imports `duckdb:extension/files-host` for remote file I/O;
// the host PROVIDES it and routes each call through the ExtensionManager to the
// registered files-backend component's `file-dispatch` export (mirroring
// storage-host above). The error channel is plain strings (not duckerror).
impl core_files_host::Host for CoreStoreState {
    fn file_open(
        &mut self,
        url: String,
    ) -> Result<core_files_host::FileOpenResult, String> {
        let mut manager = self
            .extension_manager
            .lock()
            .expect("extension manager mutex poisoned");
        manager
            .dispatch_file_open(&url)
            .map(|(handle, size)| core_files_host::FileOpenResult { handle, size })
    }

    fn file_read(
        &mut self,
        handle: u32,
        offset: u64,
        len: u32,
    ) -> Result<Vec<u8>, String> {
        let mut manager = self
            .extension_manager
            .lock()
            .expect("extension manager mutex poisoned");
        manager.dispatch_file_read(handle, offset, len)
    }

    fn file_close(&mut self, handle: u32) -> Result<(), String> {
        let mut manager = self
            .extension_manager
            .lock()
            .expect("extension manager mutex poisoned");
        manager.dispatch_file_close(handle)
    }
}

// ---- TVM spill host (Tiered Virtual Memory) ----
// Backs the libduckdb world's tvm:memory imports with an in-process region
// directory (tvm-core). DuckDB spills evicted buffer-pool blocks here via the
// wasm component's tvm_spill bridge, extending capacity past the 4 GiB wasm32
// ceiling -- the regions live in this host's 64-bit address space.

// The guest only checks Err vs Ok, so map every tvm-core error to one WIT variant.
fn tvm_err_to_wit(e: tvm_core::TvmError) -> core_tvm_types::TvmError {
    core_tvm_types::TvmError::BackingStore(format!("{e:?}"))
}
fn tvm_kind_to_core(k: core_tvm_types::RegionKind) -> tvm_core::RegionKind {
    use core_tvm_types::RegionKind as W;
    use tvm_core::RegionKind as C;
    match k {
        W::HotHeap => C::HotHeap,
        W::ObjectArena => C::ObjectArena,
        W::BlobArena => C::BlobArena,
        W::PageStore => C::PageStore,
        W::Scratch => C::Scratch,
        W::DeviceState => C::DeviceState,
        W::CodeCache => C::CodeCache,
    }
}
impl CoreStoreState {
    // Record a fresh tvm_core allocation under its (region, offset) slot, bumping
    // the per-slot generation, and return the WIT handle (carrying the slot
    // generation) to hand back to the guest.
    fn tvm_register(&mut self, region_id: u16, th: tvm_core::Handle) -> core_tvm_types::Handle {
        let slot = self.tvm_slots.entry((region_id, th.offset)).or_insert((0, None));
        slot.0 = slot.0.wrapping_add(1);
        slot.1 = Some(th);
        core_tvm_types::Handle {
            region_id,
            generation: slot.0,
            offset: th.offset,
        }
    }
    // Validate a WIT handle against its slot and return the live tvm_core handle.
    // Rejects a handle whose slot was freed (None) or freed-then-reused (the slot
    // generation moved past the handle's). `free` also marks the slot freed.
    fn tvm_resolve(
        &mut self,
        ptr: core_tvm_types::Handle,
        free: bool,
    ) -> Result<tvm_core::Handle, core_tvm_types::TvmError> {
        let slot = self
            .tvm_slots
            .get_mut(&(ptr.region_id, ptr.offset))
            .ok_or(core_tvm_types::TvmError::StaleHandle)?;
        if slot.0 != ptr.generation {
            return Err(core_tvm_types::TvmError::StaleHandle);
        }
        let th = slot.1.ok_or(core_tvm_types::TvmError::StaleHandle)?;
        if free {
            slot.1 = None;
        }
        Ok(th)
    }
}

// Opt-in observability: set DUCKDB_TVM_DEBUG=1 to trace what DuckDB spills into
// the host-owned TVM regions (region opens + cumulative bytes written/read).
fn tvm_debug() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("DUCKDB_TVM_DEBUG").is_some())
}
static TVM_REGIONS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
static TVM_BYTES_WRITTEN: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
static TVM_BYTES_READ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

impl core_tvm_manager::Host for CoreStoreState {
    fn create_region(
        &mut self,
        kind: core_tvm_types::RegionKind,
        capacity: u32,
    ) -> Result<u16, core_tvm_types::TvmError> {
        let mem = tvm_core::VecBackedRegion::new(capacity);
        // Freelist (not the default Bump): DuckDB deletes spilled blocks as a
        // sort/hash merge consumes them (tvm_spill_delete -> dealloc), and the
        // free-list coalesces those holes so a region's footprint tracks the
        // live set, not the cumulative spill volume. Bump's dealloc is a no-op.
        let r = self
            .tvm
            .create_region_with(
                tvm_kind_to_core(kind),
                capacity,
                tvm_core::AllocatorKind::Freelist,
                mem,
            )
            .map_err(tvm_err_to_wit);
        if tvm_debug() {
            if let Ok(id) = &r {
                let n = TVM_REGIONS.fetch_add(1, std::sync::atomic::Ordering::Relaxed) + 1;
                eprintln!(
                    "[tvm] open region #{n} id={id} kind={kind:?} cap={} MiB (host-owned, beyond wasm 4 GiB)",
                    capacity >> 20
                );
            }
        }
        r
    }
    fn destroy_region(&mut self, region_id: u16) -> Result<(), core_tvm_types::TvmError> {
        self.tvm.destroy_region(region_id).map_err(tvm_err_to_wit)
    }
    fn alloc(
        &mut self,
        region_id: u16,
        size: u32,
    ) -> Result<core_tvm_types::Handle, core_tvm_types::TvmError> {
        let th = self.tvm.alloc(region_id, size).map_err(tvm_err_to_wit)?;
        Ok(self.tvm_register(region_id, th))
    }
    fn dealloc(&mut self, ptr: core_tvm_types::Handle) -> Result<(), core_tvm_types::TvmError> {
        let th = self.tvm_resolve(ptr, true)?;
        self.tvm.dealloc(th).map_err(tvm_err_to_wit)
    }
    fn describe_region(
        &mut self,
        _region_id: u16,
    ) -> Result<core_tvm_types::RegionInfo, core_tvm_types::TvmError> {
        Err(core_tvm_types::TvmError::BackingStore(
            "describe-region not implemented".into(),
        ))
    }
}

impl core_tvm_bytes::Host for CoreStoreState {
    fn read(
        &mut self,
        ptr: core_tvm_types::Handle,
        len: u32,
    ) -> Result<Vec<u8>, core_tvm_types::TvmError> {
        let th = self.tvm_resolve(ptr, false)?;
        // Borrow the region bytes zero-copy, then one alloc+copy into the
        // returned Vec. Avoids the memset that `vec![0; len]` does before the
        // read would overwrite every byte anyway (a full block of needless
        // zeroing per read).
        let buf = self
            .tvm
            .region_slice_at(th, len)
            .map_err(tvm_err_to_wit)?
            .to_vec();
        if tvm_debug() {
            let t = TVM_BYTES_READ.fetch_add(len as u64, std::sync::atomic::Ordering::Relaxed)
                + len as u64;
            eprintln!("[tvm] read {len} B (cumulative {} MiB)", t >> 20);
        }
        Ok(buf)
    }
    fn write(
        &mut self,
        ptr: core_tvm_types::Handle,
        data: Vec<u8>,
    ) -> Result<(), core_tvm_types::TvmError> {
        let len = data.len() as u64;
        let th = self.tvm_resolve(ptr, false)?;
        let r = self.tvm.write(th, &data).map_err(tvm_err_to_wit);
        if tvm_debug() && r.is_ok() {
            let t = TVM_BYTES_WRITTEN.fetch_add(len, std::sync::atomic::Ordering::Relaxed) + len;
            eprintln!("[tvm] write {len} B (cumulative {} MiB)", t >> 20);
        }
        r
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

    fn with_prepared<F, R>(&mut self, f: F) -> R
    where
        F: FnOnce(
            core_db_exports::GuestPreparedStatement<'_>,
            wasmtime::StoreContextMut<'_, CoreStoreState>,
        ) -> R,
    {
        let guest = self
            .bindings
            .duckdb_component_database()
            .prepared_statement();
        let store = self.store.as_context_mut();
        f(guest, store)
    }

    fn with_appender<F, R>(&mut self, f: F) -> R
    where
        F: FnOnce(
            core_db_exports::GuestAppender<'_>,
            wasmtime::StoreContextMut<'_, CoreStoreState>,
        ) -> R,
    {
        let guest = self.bindings.duckdb_component_database().appender();
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

struct PreparedEntry {
    handle: ResourceAny,
}

struct AppenderEntry {
    handle: ResourceAny,
}

// CallbackKind / CallbackEntry / CallbackRegistry moved to the `ducklink-runtime`
// crate (imported at the top of this file).


/// Best-effort network capability policy for extension components, read from the
/// `DUCKLINK_NETWORK_GRANT` environment variable:
///   - unset / empty / "none"  -> deny every extension (default; secure)
///   - "all" / "*"             -> grant every extension
///   - otherwise               -> a comma/space-separated allowlist of names
///                                (e.g. "dns,http")
///
/// Enforcement is the WasiCtx network grant: a denied extension's wasi:sockets
/// calls fail, so it cannot reach the network even though it may still try.
fn network_grant_allows(extension: &str) -> bool {
    match std::env::var("DUCKLINK_NETWORK_GRANT") {
        Ok(v) => {
            let v = v.trim();
            if v.is_empty() || v.eq_ignore_ascii_case("none") {
                return false;
            }
            if v == "*" || v.eq_ignore_ascii_case("all") {
                return true;
            }
            v.split([',', ' '])
                .map(str::trim)
                .any(|name| !name.is_empty() && name.eq_ignore_ascii_case(extension))
        }
        Err(_) => false,
    }
}

/// Store data for a dot-command component: just wasi (the component imports it
/// for std even though the `duckdb:dotcmd` world declares no WIT imports).
struct DotcmdState {
    wasi: WasiCtx,
    table: ResourceTable,
    /// The core (for spi SQL execution) and the CLI's live connection handle.
    core: Arc<Mutex<CoreExecution>>,
    current_connection: Arc<Mutex<Option<ResourceAny>>>,
    /// PLAN-prefixes: lets `spi.query` flush staged __ducklink_prefix* rows onto
    /// the live connection before each dotcmd query (so `.prefix` sees them).
    extension_manager: Arc<Mutex<ExtensionManager>>,
    /// compose:dynlink/linker bridge state. A `DynLinkBridge` is present
    /// ONLY when this dot-command component imports `compose:dynlink/linker`
    /// (the `imports_linker` gate in `load_one`); components that don't
    /// import it carry `None` and pay nothing.
    dynlink: Option<compose_dynlink::DynLinkBridge>,
}
impl WasiView for DotcmdState {
    fn ctx(&mut self) -> WasiCtxView<'_> {
        WasiCtxView { ctx: &mut self.wasi, table: &mut self.table }
    }
}
impl wasmtime::component::HasData for DotcmdState {
    type Data<'a> = &'a mut DotcmdState;
}
impl DotcmdState {
    /// Expose the dynlink bridge for the linker Host trait impl. Only ever
    /// reached after the `imports_linker` gate set `dynlink = Some(..)`, so
    /// a guest that imports the linker always has a bridge.
    fn dynlink_bridge(&mut self) -> &mut compose_dynlink::DynLinkBridge {
        self.dynlink
            .as_mut()
            .expect("compose:dynlink/linker invoked on a dot command that did not import it")
    }
}
// Generate the compose:dynlink/linker Host + HostInstance trait impls for
// DotcmdState, delegating to its bridge (one shared implementation).
ducklink_runtime::impl_compose_dynlink_host!(DotcmdState, dynlink_bridge);

/// Process-global shared provider registry for the dot-command dlopen path.
/// Built once against the host engine; a pylon-shaped provider registered
/// here is instantiated once and shared across every dot-command guest.
fn dotcmd_provider_registry(engine: &Engine) -> &'static ProviderRegistry {
    dynlink_provider_registry(engine)
}

/// THE process-global shared `compose:dynlink` provider registry, used by
/// BOTH the dot-command path and the extension load path (so one resident
/// provider — e.g. the warmed ~38 MB pylon — serves every guest, across both
/// flavors). Built once against the host engine and populated from
/// `DUCKLINK_PROVIDERS` (see [`register_env_providers`]) on first use.
fn dynlink_provider_registry(engine: &Engine) -> &'static ProviderRegistry {
    static REG: OnceLock<ProviderRegistry> = OnceLock::new();
    REG.get_or_init(|| {
        let registry = ProviderRegistry::new(engine.clone());
        register_env_providers(&registry);
        registry
    })
}

/// Register `compose:dynlink` providers declared in the `DUCKLINK_PROVIDERS`
/// environment variable into `registry`. This mirrors `DUCKLINK_AUTOLOAD`'s
/// env-list config style.
///
/// Format (comma-separated entries; preopens are `;`-separated `guest=host`
/// pairs after a `:`):
///
/// ```text
/// DUCKLINK_PROVIDERS=pylon=/abs/pylon-endpoint-numpy.component.wasm:/lib=/abs/cpython/Lib;/app=/abs/pylib
/// ```
///
/// Each entry is `id=wasm-path[:preopens]`. A pylon provider needs
/// `/lib` (the CPython `Lib` dir incl. bundled numpy) and `/app` (the
/// dispatcher `pylib` dir) preopened into its OWN store. A provider with no
/// preopens (e.g. an echo provider) is written as just `id=path`.
///
/// Registration only COMPILES the provider; the resident instance is
/// materialized lazily on first resolve and then shared.
fn register_env_providers(registry: &ProviderRegistry) {
    let spec = match std::env::var("DUCKLINK_PROVIDERS") {
        Ok(s) if !s.trim().is_empty() => s,
        _ => return,
    };
    for entry in spec.split(',').map(str::trim).filter(|e| !e.is_empty()) {
        // Split id=path[:preopens]. The id/path boundary is the FIRST '='; the
        // path/preopens boundary is the FIRST ':' AFTER the path start (paths
        // are absolute on this platform so the leading '/' is unambiguous).
        let (id, rest) = match entry.split_once('=') {
            Some(p) => p,
            None => {
                eprintln!("[compose-dynlink] DUCKLINK_PROVIDERS: skipping malformed entry '{entry}' (expected id=path)");
                continue;
            }
        };
        let (path, preopen_spec) = match rest.split_once(":/") {
            Some((p, rest)) => (p, Some(format!("/{rest}"))),
            None => (rest, None),
        };
        let mut preopens = Vec::new();
        if let Some(po_spec) = preopen_spec {
            for pair in po_spec.split(';').map(str::trim).filter(|p| !p.is_empty()) {
                match pair.split_once('=') {
                    Some((guest, host)) => {
                        preopens.push(ProviderPreopen::new(host.trim(), guest.trim()))
                    }
                    None => eprintln!(
                        "[compose-dynlink] DUCKLINK_PROVIDERS: provider '{id}': skipping malformed preopen '{pair}' (expected guest=host)"
                    ),
                }
            }
        }
        match registry.register_provider_with_preopens(id, path.trim(), preopens.clone()) {
            Ok(()) => eprintln!(
                "[compose-dynlink] registered provider '{id}' from {} ({} preopen{})",
                path.trim(),
                preopens.len(),
                if preopens.len() == 1 { "" } else { "s" }
            ),
            Err(e) => eprintln!("[compose-dynlink] failed to register provider '{id}': {e}"),
        }
    }
}

/// `duckdb:dotcmd/spi` — run SQL on the CLI's live connection, returned as
/// tab/newline-delimited text. Shares the user's connection (temp tables,
/// `:memory:` state, settings).
impl dotcmd_bindings::duckdb::dotcmd::spi::Host for DotcmdState {
    fn query(&mut self, sql: String) -> Result<String, String> {
        let handle = self
            .current_connection
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
            .ok_or_else(|| "spi: no active database connection".to_string())?;

        // v1.1 THE PIN — the dotcmd<->host hook. `.prefix prefer/unprefer` writes
        // the pin row via SQL then issues this sentinel so the host APPLIES the
        // pins immediately (re-registers the pinned bare owners against the
        // core). The sentinel never reaches the core SQL parser.
        if sql.trim() == PREFIX_APPLY_PINS_SENTINEL {
            return self.apply_prefix_pins(handle);
        }

        // PLAN-prefixes: flush any staged __ducklink_prefix* rows onto the live
        // connection (ensures the tables exist + are populated before .prefix
        // reads them). Cheap + idempotent: only does work when rows are pending.
        let flush_sql = {
            let mut manager = self
                .extension_manager
                .lock()
                .expect("extension manager mutex poisoned");
            manager.take_prefix_table_sql()
        };
        let mut core = self.core.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(flush_sql) = flush_sql {
            if let Err(trap) =
                core.with_database(|guest, store| guest.call_execute(store, handle.clone(), &flush_sql))
            {
                eprintln!("[prefix] WARNING: failed to flush prefix tables: {trap}");
            }
        }
        let result = core
            .with_database(|guest, store| guest.call_execute(store, handle, &sql))
            .map_err(|trap| format!("spi query trapped: {trap}"))?;
        match result {
            Ok(qr) => Ok(spi_render_rows(qr)),
            Err(err) => Err(core_duckerror_message(err)),
        }
    }
}

/// v1.1 THE PIN — the dotcmd issues this exact string via `spi.query` after
/// writing the pin row; the host intercepts it (it never reaches the core SQL
/// parser) and runs the apply-pins pass.
const PREFIX_APPLY_PINS_SENTINEL: &str = "-- ducklink:prefix apply-pins";

impl DotcmdState {
    /// v1.1 THE PIN — the apply-pins pass driven from the spi sentinel. Reads
    /// `__ducklink_prefix_pin` off the live connection, hands the rows to the
    /// ExtensionManager (which refreshes its in-memory pin cache + stages
    /// re-registrations of the pinned owners), then triggers a core pending pull
    /// via `LOAD <a-loaded-extension>` so the staged re-registrations land.
    fn apply_prefix_pins(&mut self, handle: ResourceAny) -> Result<String, String> {
        let mut core = self.core.lock().unwrap_or_else(|e| e.into_inner());
        // Read the pin table from the live connection.
        let read = core
            .with_database(|guest, store| {
                guest.call_execute(
                    store,
                    handle.clone(),
                    "SELECT function_name, shape, n_args, expansion FROM __ducklink_prefix_pin",
                )
            })
            .map_err(|trap| format!("apply-pins read trapped: {trap}"))?;
        let pins: Vec<(String, prefix::Shape, i32, String)> = match read {
            Ok(qr) => qr
                .rows
                .iter()
                .filter_map(|row| {
                    let name = match row.first() {
                        Some(core_types::Duckvalue::Text(s)) => s.clone(),
                        _ => return None,
                    };
                    let shape_s = match row.get(1) {
                        Some(core_types::Duckvalue::Text(s)) => s.clone(),
                        _ => return None,
                    };
                    let shape = prefix::Shape::from_str(&shape_s)?;
                    let n_args = match row.get(2) {
                        Some(core_types::Duckvalue::Int32(n)) => *n,
                        Some(core_types::Duckvalue::Int64(n)) => *n as i32,
                        _ => return None,
                    };
                    let expansion = match row.get(3) {
                        Some(core_types::Duckvalue::Text(s)) => s.clone(),
                        _ => return None,
                    };
                    Some((name, shape, n_args, expansion))
                })
                .collect(),
            Err(err) => return Err(core_duckerror_message(err)),
        };

        // Hand the pins to the manager: refresh the cache + compute the wrapper
        // macro DDL (CREATE OR REPLACE for pins, DROP for unprefers).
        let statements = {
            let mut manager = self
                .extension_manager
                .lock()
                .expect("extension manager mutex poisoned");
            manager.apply_pins(&pins)
        };

        // Run the wrapper-macro DDL on the live connection so the pin takes
        // effect IMMEDIATELY (and survives later extension loads — a macro
        // shadows a same-name scalar in DuckDB resolution).
        for stmt in statements {
            match core.with_database(|guest, store| guest.call_execute(store, handle.clone(), &stmt)) {
                Ok(Ok(_)) => {}
                Ok(Err(err)) => {
                    return Err(format!(
                        "apply-pins: '{stmt}' failed: {}",
                        core_duckerror_message(err)
                    ))
                }
                Err(trap) => return Err(format!("apply-pins: '{stmt}' trapped: {trap}")),
            }
        }
        Ok(String::new())
    }
}

/// The human-readable message inside a core Duckerror (drops the variant noise).
fn core_duckerror_message(err: core_types::Duckerror) -> String {
    match err {
        core_types::Duckerror::Invalidargument(m)
        | core_types::Duckerror::Unsupported(m)
        | core_types::Duckerror::Invalidstate(m)
        | core_types::Duckerror::Io(m)
        | core_types::Duckerror::Internal(m) => m,
    }
}

/// Render a core query result as text: one row per line, tab-separated columns,
/// NULL as empty, no header.
fn spi_render_rows(qr: core_db_exports::QueryResult) -> String {
    let mut out = String::new();
    for row in qr.rows {
        let cells: Vec<String> = row.iter().map(spi_value_text).collect();
        out.push_str(&cells.join("\t"));
        out.push('\n');
    }
    out
}
fn spi_value_text(v: &core_types::Duckvalue) -> String {
    match v {
        core_types::Duckvalue::Null => String::new(),
        core_types::Duckvalue::Boolean(b) => b.to_string(),
        core_types::Duckvalue::Int64(n) => n.to_string(),
        core_types::Duckvalue::Uint64(n) => n.to_string(),
        core_types::Duckvalue::Float64(f) => f.to_string(),
        core_types::Duckvalue::Text(s) => s.clone(),
        core_types::Duckvalue::Blob(b) => format!("<blob {} bytes>", b.len()),
        core_types::Duckvalue::Int32(n) => n.to_string(),
        core_types::Duckvalue::Timestamp(micros) => micros.to_string(),
        core_types::Duckvalue::Int8(n) => n.to_string(),
        core_types::Duckvalue::Int16(n) => n.to_string(),
        core_types::Duckvalue::Uint8(n) => n.to_string(),
        core_types::Duckvalue::Uint16(n) => n.to_string(),
        core_types::Duckvalue::Uint32(n) => n.to_string(),
        core_types::Duckvalue::Float32(v) => v.to_string(),
        core_types::Duckvalue::Date(days) => days.to_string(),
        core_types::Duckvalue::Time(micros) => micros.to_string(),
        core_types::Duckvalue::Timestamptz(micros) => micros.to_string(),
        core_types::Duckvalue::Decimal(d) => format_decimal(d.lower, d.upper, d.width, d.scale),
        core_types::Duckvalue::Interval(iv) => {
            format!("{} months {} days {} us", iv.months, iv.days, iv.micros)
        }
        core_types::Duckvalue::Uuid(u) => format_uuid(u.hi, u.lo),
        // ESCAPE-HATCH: the value is already JSON; emit it verbatim.
        core_types::Duckvalue::Complex(c) => c.json.clone(),
    }
}

/// Render a HUGEINT-backed DECIMAL: unscaled int128 = (upper<<64 | lower),
/// inserting the decimal point `scale` digits from the right.
pub(crate) fn format_decimal(lower: u64, upper: u64, _width: u8, scale: u8) -> String {
    let raw = (((upper as u128) << 64) | lower as u128) as i128;
    let neg = raw < 0;
    let mut digits = raw.unsigned_abs().to_string();
    let scale = scale as usize;
    let s = if scale == 0 {
        digits
    } else {
        while digits.len() <= scale {
            digits.insert(0, '0');
        }
        let point = digits.len() - scale;
        format!("{}.{}", &digits[..point], &digits[point..])
    };
    if neg {
        format!("-{s}")
    } else {
        s
    }
}

/// Render a 128-bit UUID (hi/lo halves) as the canonical 8-4-4-4-12 hex form.
pub(crate) fn format_uuid(hi: u64, lo: u64) -> String {
    let v = ((hi as u128) << 64) | lo as u128;
    let h = format!("{v:032x}");
    format!(
        "{}-{}-{}-{}-{}",
        &h[0..8],
        &h[8..12],
        &h[12..16],
        &h[16..20],
        &h[20..32]
    )
}

/// A loaded pluggable dot-command component (its own wasmtime store + instance).
struct DotcmdInstance {
    store: Store<DotcmdState>,
    bindings: dotcmd_bindings::Dotcmd,
}

/// Registry of pluggable dot-command components. Each declares its commands via
/// `registry.list-commands`; the host routes `.NAME args` typed at the CLI to the
/// owning component's `registry.invoke`.
pub struct DotcmdRegistry {
    components: Vec<DotcmdInstance>,
    /// lowercased command name -> (component index, command id)
    by_name: HashMap<String, (usize, u64)>,
    /// (name, summary, usage) for every command, sorted by name — for `.help`.
    infos: Vec<(String, String, String)>,
}

impl DotcmdRegistry {
    /// Every registered command (name, summary, usage), sorted by name.
    fn list_commands(&self) -> Vec<(String, String, String)> {
        self.infos.clone()
    }

    /// Load every `*.wasm` dot-command component in `dir` (missing dir = empty).
    /// `core` + `current_connection` back the spi import (SQL on the live conn).
    fn load(
        engine: &Engine,
        dir: &Path,
        core: Arc<Mutex<CoreExecution>>,
        current_connection: Arc<Mutex<Option<ResourceAny>>>,
        extension_manager: Arc<Mutex<ExtensionManager>>,
    ) -> Self {
        let mut components = Vec::new();
        let mut by_name = HashMap::new();
        let mut infos: Vec<(String, String, String)> = Vec::new();
        let mut paths: Vec<PathBuf> = std::fs::read_dir(dir)
            .into_iter()
            .flatten()
            .flatten()
            .map(|e| e.path())
            .filter(|p| p.extension().and_then(|s| s.to_str()) == Some("wasm"))
            .collect();
        paths.sort();
        for path in paths {
            match Self::load_one(
                engine,
                &path,
                core.clone(),
                current_connection.clone(),
                extension_manager.clone(),
            ) {
                Ok((inst, specs)) => {
                    let idx = components.len();
                    let names: Vec<String> = specs.iter().map(|(n, ..)| n.clone()).collect();
                    for (name, id, summary, usage) in specs {
                        by_name
                            .entry(name.to_ascii_lowercase())
                            .or_insert((idx, id));
                        infos.push((name, summary, usage));
                    }
                    eprintln!(
                        "[dotcmd] loaded {} -> .{}",
                        path.file_name().and_then(|s| s.to_str()).unwrap_or("?"),
                        names.join(", .")
                    );
                    components.push(inst);
                }
                Err(err) => eprintln!("[dotcmd] failed to load {}: {err:?}", path.display()),
            }
        }
        infos.sort();
        Self { components, by_name, infos }
    }

    fn load_one(
        engine: &Engine,
        path: &Path,
        core: Arc<Mutex<CoreExecution>>,
        current_connection: Arc<Mutex<Option<ResourceAny>>>,
        extension_manager: Arc<Mutex<ExtensionManager>>,
    ) -> wasmtime::Result<(DotcmdInstance, Vec<(String, u64, String, String)>)> {
        let component = load_component(engine, path).map_err(wasmtime::Error::msg)?;
        let mut linker = Linker::<DotcmdState>::new(engine);
        p2::add_to_linker_sync(&mut linker)?;
        dotcmd_bindings::duckdb::dotcmd::spi::add_to_linker::<DotcmdState, DotcmdState>(
            &mut linker,
            |s| s,
        )?;
        // compose:dynlink/linker: conditionally satisfy a guest-driven
        // dlopen import. ONLY components that actually import the linker get
        // the host import + a bridge — every other dot command is unaffected
        // and pays nothing (the gate mirrors the framework's `imports_linker`).
        let imports_dynlink = compose_dynlink::imports_linker(engine, &component);
        let dynlink = if imports_dynlink {
            eprintln!(
                "[dotcmd] '{}' imports compose:dynlink/linker; wiring the shared-provider bridge",
                path.display()
            );
            compose_dynlink::add_to_linker::<DotcmdState>(&mut linker)
                .map_err(|e| wasmtime::Error::msg(e.to_string()))?;
            // A per-loader provider registry: empty until a provider is
            // registered (`ExtensionManager::register_dynlink_provider`).
            Some(compose_dynlink::DynLinkBridge::new(
                dotcmd_provider_registry(engine).clone(),
            ))
        } else {
            None
        };
        let wasi = WasiCtxBuilder::new().inherit_stdio().build();
        let mut store = Store::new(
            engine,
            DotcmdState {
                wasi,
                table: ResourceTable::new(),
                core,
                current_connection,
                extension_manager,
                dynlink,
            },
        );
        let bindings = dotcmd_bindings::Dotcmd::instantiate(&mut store, &component, &linker)?;
        let specs = bindings
            .duckdb_dotcmd_registry()
            .call_list_commands(&mut store)?
            .into_iter()
            .map(|s| (s.name, s.id, s.summary, s.usage))
            .collect();
        Ok((DotcmdInstance { store, bindings }, specs))
    }

    /// Invoke `.name args`. None = no registered command by that name (the CLI
    /// then falls back to its built-ins). Ok = (text-to-print, state-deltas as
    /// (key,value) pairs); Err = a graceful error message.
    fn invoke(
        &mut self,
        name: &str,
        args: &str,
    ) -> Option<Result<(String, Vec<(String, String)>), String>> {
        let (idx, id) = *self.by_name.get(&name.to_ascii_lowercase())?;
        let inst = &mut self.components[idx];
        Some(
            match inst
                .bindings
                .duckdb_dotcmd_registry()
                .call_invoke(&mut inst.store, id, args)
            {
                Ok(Ok(result)) => Ok((
                    result.text,
                    result
                        .state_deltas
                        .into_iter()
                        .map(|d| (d.key, d.value))
                        .collect(),
                )),
                Ok(Err(message)) => Err(message),
                Err(trap) => Err(format!("dot-command '{name}' trapped: {trap}")),
            },
        )
    }
}

/// Directory holding pluggable dot-command components (sibling of the extension
/// root; default `artifacts/dotcmds`).
fn dotcmd_root() -> PathBuf {
    EXTENSION_ROOT
        .get()
        .and_then(|p| p.parent().map(|d| d.join("dotcmds")))
        .unwrap_or_else(|| workspace_root().join("artifacts/dotcmds"))
}

/// Snapshot all registered dot commands as CLI `command-info` records (for `.help`).
fn cli_command_infos(
    store: &StoreContextMut<'_, HostState>,
) -> Vec<duckdb_cli_bindings::duckdb::cli::dotcmd_host::CommandInfo> {
    let registry = store.data().dotcmd_registry.clone();
    let registry = registry.lock().unwrap_or_else(|e| e.into_inner());
    registry
        .list_commands()
        .into_iter()
        .map(
            |(name, summary, usage)| duckdb_cli_bindings::duckdb::cli::dotcmd_host::CommandInfo {
                name,
                summary,
                usage,
            },
        )
        .collect()
}

/// Build the CLI-facing dotcmd outcome (text + state-deltas) the func_wrap returns.
fn make_cli_outcome(
    text: String,
    deltas: Vec<(String, String)>,
) -> duckdb_cli_bindings::duckdb::cli::dotcmd_host::Outcome {
    duckdb_cli_bindings::duckdb::cli::dotcmd_host::Outcome {
        text,
        state_deltas: deltas
            .into_iter()
            .map(
                |(key, value)| duckdb_cli_bindings::duckdb::cli::dotcmd_host::StateDelta {
                    key,
                    value,
                },
            )
            .collect(),
    }
}

struct ExtensionManager {
    engine: Engine,
    core: Option<Arc<Mutex<CoreExecution>>>,
    extensions: HashMap<String, ExtensionInstance>,
    callback_registry: Arc<Mutex<CallbackRegistry>>,
    // M2a: registered ATTACH storage backends, captured from each extension's
    // `register-storage`. Keyed by ATTACH TYPE name (e.g. "sqlitewasm"); the
    // value is the backing extension name + the callback-handle the component
    // expects on every storage-dispatch call.
    storage_backends: HashMap<String, (String, u32)>,
    // Item 3 / M2a: registered custom INDEX TYPE backends, captured from each
    // extension's `register-index-type`. Keyed by index TYPE name (e.g.
    // "wasm_hnsw"); the value is the backing extension name. The core pulls the
    // type names (via index-host.index-type-list) and registers a wasm IndexType
    // for each, so `CREATE INDEX ... USING <type>` dispatches here.
    index_backends: HashMap<String, String>,
    // httpfs M2: the single registered files backend (the component that backs
    // http(s):// reads), as (extension name, callback-handle). Captured from a
    // component's `files-reg.register-files` at load.
    files_backend: Option<(String, u32)>,
    // Item 2: collations components have declared via `collation.register-collation`.
    // The core pulls this list (through the `collation-host.collation-list`
    // import) and wraps each as a DuckDB collation reusing the named sort-key
    // scalar. Keyed by collation name -> (transform scalar, combinable).
    collations: HashMap<String, (String, bool)>,
    // Item 4: pragmas components have declared via `runtime.pragma-registry.register-call`.
    // The core pulls this list (through the `pragma-host.pragma-list` import) and
    // intercepts `PRAGMA <name>(...)`, dispatching via the callback handle (the
    // component returns a SQL script the core runs). Keyed by pragma name ->
    // (extension, callback-handle).
    pragmas: HashMap<String, (String, u32)>,
    // Function prefixes (PLAN-prefixes): the registry/index.json name ->
    // {prefix, expansion} map loaded at host start, used to namespace every
    // scalar/table/aggregate registration as `prefix__name`.
    prefix_registry: prefix::PrefixRegistry,
    // Cross-component bare-name collision tracker; drives the load-time warning.
    prefix_collisions: prefix::CollisionTracker,
    // The function rows recorded into __ducklink_prefix_function so far this
    // session, so the host only emits each INSERT once:
    // (expansion, function_name, shape, n_args).
    prefix_recorded: std::collections::HashSet<(String, String, &'static str, i32)>,
    // Staged prefix rows awaiting a flush to the live connection (built during
    // the pure registration drain; written by `flush_prefix_tables`).
    pending_prefix_rows: Vec<prefix::PrefixRow>,
    // v1.1 THE PIN: every bare registration def seen this session, keyed by
    // (name, shape, n_args, expansion). Lets the host RE-REGISTER a specific
    // extension's bare function on demand (`.prefix prefer`) and revert to the
    // default last-loaded owner (`.prefix unprefer`).
    prefix_retained: prefix::RetainedDefs,
    // v1.1 THE PIN: in-memory mirror of __ducklink_prefix_pin, refreshed by the
    // spi `apply_pins` pass. (name, shape, n_args) -> pinned expansion. Drives
    // the unprefer diff (which wrapper macros to DROP). Load-order independence
    // is automatic — the wrapper macro shadows any later bare-scalar
    // re-registration — so no mid-drain honor pass is needed.
    pin_cache: HashMap<(String, prefix::Shape, i32), String>,
    // v1.1 live-query host import: the CLI's live connection, shared so a
    // query-capable component's `query` import (catalog completion) runs on the
    // same connection the user is on. Cloned into each component's CoreServices.
    current_connection: Arc<Mutex<Option<ResourceAny>>>,
    // v1.1 live-query host import: the re-entrancy fallback catalog snapshot,
    // shared with each component's CoreServices + refreshed at CLI boundaries.
    catalog_snapshot: Arc<Mutex<CatalogSnapshot>>,
}

impl ExtensionManager {
    fn new(engine: Engine) -> Self {
        let index_path = workspace_root().join("registry/index.json");
        let prefix_registry = prefix::PrefixRegistry::load_from_index(&index_path);
        Self {
            engine,
            core: None,
            extensions: HashMap::new(),
            callback_registry: Arc::new(Mutex::new(CallbackRegistry::new())),
            storage_backends: HashMap::new(),
            index_backends: HashMap::new(),
            files_backend: None,
            collations: HashMap::new(),
            pragmas: HashMap::new(),
            prefix_registry,
            prefix_collisions: prefix::CollisionTracker::default(),
            prefix_recorded: std::collections::HashSet::new(),
            pending_prefix_rows: Vec::new(),
            prefix_retained: prefix::RetainedDefs::default(),
            pin_cache: HashMap::new(),
            current_connection: Arc::new(Mutex::new(None)),
            catalog_snapshot: Arc::new(Mutex::new(CatalogSnapshot::default())),
        }
    }

    /// PLAN-prefixes core: for every scalar/table/aggregate registration, also
    /// emit a duplicate under the qualified name `{prefix}__{name}` (same
    /// callback handle, same args/returns/options) so the function is callable
    /// both ways. Bare names keep DuckDB's last-registered-wins behavior; the
    /// qualified form is always unique. Warns on cross-component bare-name
    /// collisions and stages the __ducklink_prefix* rows for a later flush.
    fn apply_function_prefixes(&mut self, data: &mut PendingRegistrationsData) {
        use prefix::Shape;

        // SCALARS
        let mut qualified_scalars: Vec<reg::ScalarReg> = Vec::new();
        for entry in &data.scalars {
            let n_args = entry.arguments.len() as i32;
            let info = self.prefix_registry.resolve(&entry.extension);
            self.note_prefix_registration(
                &entry.extension,
                &entry.name,
                Shape::Scalar,
                n_args,
                &info,
            );
            // Retain the bare def so a pin can resurrect THIS expansion's impl.
            self.prefix_retained.insert(
                &info.expansion,
                &info.prefix,
                &entry.name,
                Shape::Scalar,
                n_args,
                prefix::RetainedDef::Scalar(entry.clone()),
            );
            if let Some(qname) = prefix::qualified_name(&info.prefix, &entry.name) {
                let mut dup = entry.clone();
                dup.name = qname;
                qualified_scalars.push(dup);
            }
        }
        data.scalars.extend(qualified_scalars);

        // TABLES
        let mut qualified_tables: Vec<reg::TableReg> = Vec::new();
        for entry in &data.tables {
            let n_args = entry.arguments.len() as i32;
            let info = self.prefix_registry.resolve(&entry.extension);
            self.note_prefix_registration(
                &entry.extension,
                &entry.name,
                Shape::Table,
                n_args,
                &info,
            );
            self.prefix_retained.insert(
                &info.expansion,
                &info.prefix,
                &entry.name,
                Shape::Table,
                n_args,
                prefix::RetainedDef::Table(entry.clone()),
            );
            if let Some(qname) = prefix::qualified_name(&info.prefix, &entry.name) {
                let mut dup = entry.clone();
                dup.name = qname;
                qualified_tables.push(dup);
            }
        }
        data.tables.extend(qualified_tables);

        // AGGREGATES
        let mut qualified_aggregates: Vec<reg::AggregateReg> = Vec::new();
        for entry in &data.aggregates {
            let n_args = entry.arguments.len() as i32;
            let info = self.prefix_registry.resolve(&entry.extension);
            self.note_prefix_registration(
                &entry.extension,
                &entry.name,
                Shape::Aggregate,
                n_args,
                &info,
            );
            self.prefix_retained.insert(
                &info.expansion,
                &info.prefix,
                &entry.name,
                Shape::Aggregate,
                n_args,
                prefix::RetainedDef::Aggregate(entry.clone()),
            );
            if let Some(qname) = prefix::qualified_name(&info.prefix, &entry.name) {
                let mut dup = entry.clone();
                dup.name = qname;
                qualified_aggregates.push(dup);
            }
        }
        data.aggregates.extend(qualified_aggregates);

        // MACROS (v1.1): a macro is dispatched by NAME (it becomes a DuckDB
        // CREATE MACRO), so `{prefix}__{name}` namespacing applies identically.
        // Arity is the parameter count.
        let mut qualified_macros: Vec<reg::MacroReg> = Vec::new();
        for entry in &data.macros {
            let n_args = entry.parameters.len() as i32;
            let info = self.prefix_registry.resolve(&entry.extension);
            self.note_prefix_registration(
                &entry.extension,
                &entry.name,
                Shape::Macro,
                n_args,
                &info,
            );
            self.prefix_retained.insert(
                &info.expansion,
                &info.prefix,
                &entry.name,
                Shape::Macro,
                n_args,
                prefix::RetainedDef::Macro(entry.clone()),
            );
            if let Some(qname) = prefix::qualified_name(&info.prefix, &entry.name) {
                let mut dup = entry.clone();
                dup.name = qname;
                qualified_macros.push(dup);
            }
        }
        data.macros.extend(qualified_macros);

        // --- DELIBERATELY OUT OF SCOPE for prefix namespacing ---
        // CAST is keyed by (from_type, to_type), NOT called by a name, so there
        // is no `prefix__name` call surface — a `jsonfns__<cast>` is
        // meaningless. STORAGE / FILES / INDEX are keyed by an ATTACH TYPE name
        // / URL scheme / index-type name; those collide on TYPE/scheme strings,
        // not on a function name, and `prefix__sqlitewasm` as an ATTACH TYPE is
        // nonsensical. These shapes need a different collision surface and are
        // intentionally NOT prefixed here. (See PLAN-prefixes "Out of scope".)
    }

    /// Record one registration into the collision tracker (warning on a
    /// cross-component bare-name clash) and stage its __ducklink_prefix* rows.
    fn note_prefix_registration(
        &mut self,
        extension: &str,
        bare_name: &str,
        shape: prefix::Shape,
        n_args: i32,
        info: &prefix::PrefixInfo,
    ) {
        let report = self.prefix_collisions.record(
            extension,
            &info.expansion,
            info,
            bare_name,
            shape,
            n_args,
        );
        if report.is_collision {
            eprintln!("{}", prefix::format_collision_warning(&report));
        }
        let dedup_key = (
            info.expansion.clone(),
            bare_name.to_string(),
            shape.as_str(),
            n_args,
        );
        if self.prefix_recorded.insert(dedup_key) {
            self.pending_prefix_rows.push(prefix::PrefixRow {
                prefix: info.prefix.clone(),
                expansion: info.expansion.clone(),
                extension: extension.to_string(),
                function_name: bare_name.to_string(),
                shape: shape.as_str(),
                n_args,
            });
        }
    }

    /// v1.1: for each captured collation, also register a qualified
    /// `{prefix}__{name}` collation into the `collations` map (the core pulls it
    /// by name), track collisions, stage prefix rows, and retain the bare def.
    /// Collations carry no call args, so the arity key is 0.
    fn prefix_collations(&mut self, collations: &[reg::CollationReg]) {
        use prefix::Shape;
        let mut qualified: Vec<(String, (String, bool))> = Vec::new();
        for c in collations {
            let info = self.prefix_registry.resolve(&c.extension);
            self.note_prefix_registration(&c.extension, &c.name, Shape::Collation, 0, &info);
            self.prefix_retained.insert(
                &info.expansion,
                &info.prefix,
                &c.name,
                Shape::Collation,
                0,
                prefix::RetainedDef::Collation(c.name.clone(), c.transform_scalar.clone(), c.combinable),
            );
            if let Some(qname) = prefix::qualified_name(&info.prefix, &c.name) {
                qualified.push((qname, (c.transform_scalar.clone(), c.combinable)));
            }
        }
        for (qname, val) in qualified {
            self.collations.insert(qname, val);
        }
    }

    /// v1.1: for each captured pragma, also register a qualified
    /// `{prefix}__{name}` pragma into the `pragmas` map (the core intercepts the
    /// qualified name too), track collisions, stage prefix rows, and retain the
    /// bare def. Pragmas are variadic, so the arity key is -1.
    fn prefix_pragmas(&mut self, pragmas: &[reg::PragmaReg]) {
        use prefix::Shape;
        let mut qualified: Vec<(String, (String, u32))> = Vec::new();
        for p in pragmas {
            let info = self.prefix_registry.resolve(&p.extension);
            self.note_prefix_registration(&p.extension, &p.name, Shape::Pragma, -1, &info);
            self.prefix_retained.insert(
                &info.expansion,
                &info.prefix,
                &p.name,
                Shape::Pragma,
                -1,
                prefix::RetainedDef::Pragma(p.name.clone(), p.extension.clone(), p.callback_handle),
            );
            if let Some(qname) = prefix::qualified_name(&info.prefix, &p.name) {
                qualified.push((qname, (p.extension.clone(), p.callback_handle)));
            }
        }
        for (qname, val) in qualified {
            self.pragmas.insert(qname, val);
        }
    }

    /// v1.1 THE PIN — re-register the pinned expansion's bare def for one
    /// (name, shape, n_args), making that impl own the bare name NOW.
    ///
    /// THE MECHANISM (host-only, no core rebuild): the qualified form
    /// `{prefix}__{name}` is ALWAYS registered (additive v1 behavior), so we
    /// make the BARE name dispatch to the pinned impl by creating a wrapper
    /// `CREATE OR REPLACE MACRO {name}(args) AS ({prefix}__{name}(args))`. A
    /// macro shadows a same-name scalar/aggregate/macro in DuckDB resolution, so
    /// this:
    ///   * takes effect immediately on the connection,
    ///   * SURVIVES a later extension loading + re-registering the bare scalar
    ///     (the macro still shadows it) — load-order independence for free,
    ///   * reverts cleanly on `DROP MACRO` (the bare scalar resurfaces).
    /// (DuckDB's ALTER_ON_CONFLICT means a re-forwarded bare *function*
    /// registration would also last-wins, but the core only re-pulls
    /// registrations on a FRESH `LOAD`, which DuckDB short-circuits for an
    /// already-loaded extension — so the macro wrapper is the reliable
    /// host-side lever.)
    ///
    /// Returns the SQL to run, or `None` if the pinned (name,shape,arity,
    /// expansion) has no retained def (extension not loaded) — logged + ignored.
    /// Collation/pragma pins are recorded but not macro-wrappable (they are not
    /// called as `name(args)`); for those the pin row stands and the qualified
    /// `prefix__name` form is the disambiguation surface.
    fn pin_macro_sql(
        &self,
        name: &str,
        shape: prefix::Shape,
        n_args: i32,
        expansion: &str,
    ) -> Option<String> {
        if matches!(shape, prefix::Shape::Collation | prefix::Shape::Pragma) {
            // Not call-by-`name(args)`: no macro wrapper. The qualified form is
            // the disambiguation surface; the pin row is advisory.
            return None;
        }
        let def = match self.prefix_retained.get(name, shape, n_args, expansion) {
            Some(d) => d,
            None => {
                eprintln!(
                    "[prefix] WARNING: pin for '{name}' ({}/{n_args}-arg) -> '{expansion}' has no retained def (extension not loaded?); ignored",
                    shape.as_str()
                );
                return None;
            }
        };
        let prefix = self.prefix_retained.prefix_for(expansion)?;
        let qname = prefix::qualified_name(prefix, name)
            .unwrap_or_else(|| format!("{prefix}__{name}"));
        // Build the macro parameter list. Use the retained def's declared arg
        // names when present, else positional p0..pN. Variadic (-1) wrappers
        // are skipped (a macro can't forward an unknown arity).
        let params = match def {
            prefix::RetainedDef::Scalar(r) => Some(macro_param_names(&r.arguments)),
            prefix::RetainedDef::Aggregate(r) => Some(macro_param_names(&r.arguments)),
            prefix::RetainedDef::Table(_) => None, // table macros differ; skip
            prefix::RetainedDef::Macro(r) => Some(r.parameters.clone()),
            _ => None,
        }?;
        let param_list = params.join(", ");
        eprintln!(
            "[prefix] PIN: bare '{name}' ({}/{n_args}-arg) -> '{expansion}' via macro alias -> {qname}",
            shape.as_str()
        );
        Some(format!(
            "CREATE OR REPLACE MACRO {name}({param_list}) AS ({qname}({param_list}));"
        ))
    }

    /// v1.1 THE PIN — the apply-pins pass. Given the CURRENT rows of
    /// `__ducklink_prefix_pin` (function_name, shape, n_args, expansion),
    /// returns the SQL the caller runs on the live connection:
    ///   * `CREATE OR REPLACE MACRO` for each pinned key (the pin wins now);
    ///   * `DROP MACRO IF EXISTS` for any key that WAS pinned but is no longer
    ///     in the table (an `unprefer`) — the bare scalar/aggregate resurfaces.
    /// Refreshes the in-memory `pin_cache` to the new set (also read at
    /// load-time by `drain_pending_registrations` for the honor pass).
    fn apply_pins(&mut self, pins: &[(String, prefix::Shape, i32, String)]) -> Vec<String> {
        let mut sql: Vec<String> = Vec::new();
        let new_set: HashMap<(String, prefix::Shape, i32), String> = pins
            .iter()
            .map(|(name, shape, n_args, expansion)| {
                ((name.clone(), *shape, *n_args), expansion.clone())
            })
            .collect();
        // Unpinned keys -> drop the wrapper macro to revert to the bare scalar.
        let removed: Vec<(String, prefix::Shape, i32)> = self
            .pin_cache
            .keys()
            .filter(|k| !new_set.contains_key(*k))
            .cloned()
            .collect();
        for (name, shape, _n_args) in removed {
            if matches!(shape, prefix::Shape::Collation | prefix::Shape::Pragma) {
                continue;
            }
            eprintln!("[prefix] UNPIN: dropping wrapper macro for bare '{name}' (revert to last-loaded)");
            sql.push(format!("DROP MACRO IF EXISTS {name};"));
        }
        // Pinned keys -> (re)create the wrapper macro.
        for (name, shape, n_args, expansion) in pins {
            if let Some(stmt) = self.pin_macro_sql(name, *shape, *n_args, expansion) {
                sql.push(stmt);
            }
        }
        self.pin_cache = new_set;
        sql
    }

    /// Drain the staged __ducklink_prefix* rows as the SQL needed to upsert
    /// them. The caller runs this against the live connection (a safe point,
    /// outside the core's registration hook). Empty when nothing is pending.
    fn take_prefix_table_sql(&mut self) -> Option<String> {
        if self.pending_prefix_rows.is_empty() {
            return None;
        }
        let rows = std::mem::take(&mut self.pending_prefix_rows);
        Some(prefix::build_prefix_table_sql(&rows))
    }

    /// Item 2: the collations components have declared (via `register-collation`),
    /// as (name, transform-scalar, combinable). The core pulls this through the
    /// `collation-host.collation-list` import and wraps each as a DuckDB collation.
    fn registered_collations(&self) -> Vec<(String, String, bool)> {
        self.collations
            .iter()
            .map(|(name, (scalar, combinable))| (name.clone(), scalar.clone(), *combinable))
            .collect()
    }

    /// Item 4: the pragmas components have declared (via `register-call`), as
    /// (name, callback-handle). The core pulls this through the
    /// `pragma-host.pragma-list` import and intercepts `PRAGMA <name>(...)`.
    fn registered_pragmas(&self) -> Vec<(String, u32)> {
        self.pragmas
            .iter()
            .map(|(name, (_extension, handle))| (name.clone(), *handle))
            .collect()
    }

    /// The ATTACH `TYPE` names of every storage backend a component has
    /// registered (via `register-storage`). The core pulls this list (through the
    /// `storage-host.storage-list-types` import) and registers a wasm
    /// StorageExtension for each, so `ATTACH ... (TYPE <name>)` dispatches here.
    fn registered_storage_types(&self) -> Vec<String> {
        self.storage_backends.keys().cloned().collect()
    }

    /// Resolve the storage backend that should service an ATTACH. For M2a the
    /// type name is hardcoded "sqlitewasm" core-side, so prefer that backend and
    /// otherwise fall back to the single registered backend (if unambiguous).
    fn resolve_storage_backend(&self) -> Result<(String, u32), extension_types::Duckerror> {
        if let Some((ext, handle)) = self.storage_backends.get("sqlitewasm") {
            return Ok((ext.clone(), *handle));
        }
        if self.storage_backends.len() == 1 {
            let (ext, handle) = self.storage_backends.values().next().unwrap();
            return Ok((ext.clone(), *handle));
        }
        // Multiple type keys may alias the SAME backing extension (e.g. a
        // backend that registers both "mysql" and "mysqlwasm"). If every key
        // resolves to one extension, that backend is still unambiguous.
        {
            let mut iter = self.storage_backends.values();
            if let Some(first) = iter.next() {
                if iter.all(|v| v.0 == first.0) {
                    return Ok((first.0.clone(), first.1));
                }
            }
        }
        Err(extension_types::Duckerror::Invalidstate(format!(
            "no storage backend registered for 'sqlitewasm' (have {} backend(s))",
            self.storage_backends.len()
        )))
    }

    /// Reads the foreign DB file at `dsn`, stages it into the backing component,
    /// and opens the catalog; returns the component-side catalog handle.
    fn dispatch_storage_attach(
        &mut self,
        dsn: &str,
    ) -> Result<u32, extension_types::Duckerror> {
        let (ext, handle) = self.resolve_storage_backend()?;
        eprintln!("[storage-attach] dispatch_storage_attach ext='{ext}' dsn='{dsn}'");
        // The dsn may be a FILE (sqlite-over-blob) or a CONNECTION STRING
        // (e.g. mysql `host=... user=...`). Staging bytes via attach-blob is
        // BEST-EFFORT: only when the dsn names an existing readable file. For a
        // connection-string backend (mysql) the file read is skipped and the
        // component's storage-attach receives the raw dsn to dial directly.
        let bytes = match std::fs::metadata(dsn) {
            Ok(m) if m.is_file() => std::fs::read(dsn).map_err(|e| {
                extension_types::Duckerror::Io(format!("cannot read attach file '{dsn}': {e}"))
            })?,
            _ => Vec::new(),
        };
        let instance = self.extensions.get_mut(&ext).ok_or_else(|| {
            extension_types::Duckerror::Invalidstate(format!("storage extension '{ext}' not loaded"))
        })?;
        instance.storage_attach(handle, dsn, &bytes)
    }

    fn dispatch_storage_list_tables(
        &mut self,
        catalog: u32,
    ) -> Result<Vec<String>, extension_types::Duckerror> {
        let (ext, handle) = self.resolve_storage_backend()?;
        let instance = self.extensions.get_mut(&ext).ok_or_else(|| {
            extension_types::Duckerror::Invalidstate(format!("storage extension '{ext}' not loaded"))
        })?;
        instance.storage_list_tables(handle, catalog)
    }

    fn dispatch_storage_table_columns(
        &mut self,
        catalog: u32,
        table: &str,
    ) -> Result<Vec<extension_types::Columndef>, extension_types::Duckerror> {
        let (ext, handle) = self.resolve_storage_backend()?;
        let instance = self.extensions.get_mut(&ext).ok_or_else(|| {
            extension_types::Duckerror::Invalidstate(format!("storage extension '{ext}' not loaded"))
        })?;
        instance.storage_table_columns(handle, catalog, table)
    }

    fn dispatch_storage_scan_open(
        &mut self,
        catalog: u32,
        request: storage_scan::ScanRequest,
    ) -> Result<u32, extension_types::Duckerror> {
        let (ext, handle) = self.resolve_storage_backend()?;
        let instance = self.extensions.get_mut(&ext).ok_or_else(|| {
            extension_types::Duckerror::Invalidstate(format!("storage extension '{ext}' not loaded"))
        })?;
        instance.storage_scan_open(handle, catalog, request)
    }

    fn dispatch_storage_scan_next(
        &mut self,
        scan: u32,
        max_rows: u32,
    ) -> Result<Vec<Vec<extension_types::Duckvalue>>, extension_types::Duckerror> {
        let (ext, handle) = self.resolve_storage_backend()?;
        let instance = self.extensions.get_mut(&ext).ok_or_else(|| {
            extension_types::Duckerror::Invalidstate(format!("storage extension '{ext}' not loaded"))
        })?;
        instance.storage_scan_next(handle, scan, max_rows)
    }

    fn dispatch_storage_scan_close(
        &mut self,
        scan: u32,
    ) -> Result<bool, extension_types::Duckerror> {
        let (ext, handle) = self.resolve_storage_backend()?;
        let instance = self.extensions.get_mut(&ext).ok_or_else(|| {
            extension_types::Duckerror::Invalidstate(format!("storage extension '{ext}' not loaded"))
        })?;
        instance.storage_scan_close(handle, scan)
    }

    // --- Item 3 / M2a: custom index (build + search) routing ---

    /// The custom index TYPE names every component has registered (via
    /// `register-index-type`). The core pulls this list (through the
    /// `index-host.index-type-list` import) and registers a wasm IndexType for
    /// each, so `CREATE INDEX ... USING <type>` dispatches here.
    fn registered_index_types(&self) -> Vec<String> {
        self.index_backends.keys().cloned().collect()
    }

    /// Resolve the index backend that should service a `(type_name)` index
    /// operation. Prefer the exact type-name match; otherwise fall back to the
    /// single registered index backend (if unambiguous).
    fn resolve_index_backend(
        &self,
        type_name: &str,
    ) -> Result<String, extension_types::Duckerror> {
        if let Some(ext) = self.index_backends.get(type_name) {
            return Ok(ext.clone());
        }
        if self.index_backends.len() == 1 {
            return Ok(self.index_backends.values().next().unwrap().clone());
        }
        {
            let mut iter = self.index_backends.values();
            if let Some(first) = iter.next() {
                if iter.all(|v| v == first) {
                    return Ok(first.clone());
                }
            }
        }
        Err(extension_types::Duckerror::Invalidstate(format!(
            "no index backend registered for '{type_name}' (have {} backend(s))",
            self.index_backends.len()
        )))
    }

    fn dispatch_index_create(
        &mut self,
        type_name: &str,
        index_name: &str,
        dims: u32,
    ) -> Result<u32, extension_types::Duckerror> {
        let ext = self.resolve_index_backend(type_name)?;
        eprintln!(
            "[index-create] dispatch_index_create ext='{ext}' type='{type_name}' name='{index_name}' dims={dims}"
        );
        let instance = self.extensions.get_mut(&ext).ok_or_else(|| {
            extension_types::Duckerror::Invalidstate(format!("index extension '{ext}' not loaded"))
        })?;
        instance.index_create(type_name, index_name, dims)
    }

    fn dispatch_index_append(
        &mut self,
        handle: u32,
        rowids: &[i64],
        vectors: &[Vec<f32>],
    ) -> Result<(), extension_types::Duckerror> {
        // The build pipeline targets the single resolved index backend (M2a: one
        // index extension at a time). Resolve by the empty type (falls back to the
        // single registered backend).
        let ext = self.resolve_index_backend("")?;
        let instance = self.extensions.get_mut(&ext).ok_or_else(|| {
            extension_types::Duckerror::Invalidstate(format!("index extension '{ext}' not loaded"))
        })?;
        instance.index_append(handle, rowids, vectors)
    }

    fn dispatch_index_build(&mut self, handle: u32) -> Result<(), extension_types::Duckerror> {
        let ext = self.resolve_index_backend("")?;
        let instance = self.extensions.get_mut(&ext).ok_or_else(|| {
            extension_types::Duckerror::Invalidstate(format!("index extension '{ext}' not loaded"))
        })?;
        instance.index_build(handle)
    }

    fn dispatch_index_search(
        &mut self,
        handle: u32,
        query: &[f32],
        k: u32,
    ) -> Result<Vec<ducklink_runtime::extension::IndexHit>, extension_types::Duckerror> {
        let ext = self.resolve_index_backend("")?;
        let instance = self.extensions.get_mut(&ext).ok_or_else(|| {
            extension_types::Duckerror::Invalidstate(format!("index extension '{ext}' not loaded"))
        })?;
        instance.index_search(handle, query, k)
    }

    fn dispatch_index_drop(&mut self, handle: u32) -> Result<(), extension_types::Duckerror> {
        let ext = self.resolve_index_backend("")?;
        let instance = self.extensions.get_mut(&ext).ok_or_else(|| {
            extension_types::Duckerror::Invalidstate(format!("index extension '{ext}' not loaded"))
        })?;
        instance.index_drop(handle)
    }

    // httpfs M2: route file-open/read/close to the registered files backend's
    // file-dispatch export. The error channel is plain strings (surfaced to the
    // core's WasmFileSystem as an IOException message).

    /// Resolve the files backend (extension name + callback-handle). Errors with
    /// a clear message when no files component is loaded, so `http://` without
    /// `LOAD webfs` fails cleanly.
    fn resolve_files_backend(&self) -> Result<(String, u32), String> {
        self.files_backend
            .clone()
            .ok_or_else(|| "no files backend loaded (LOAD a files extension, e.g. webfs)".to_string())
    }

    fn dispatch_file_open(&mut self, url: &str) -> Result<(u32, u64), String> {
        let (ext, handle) = self.resolve_files_backend()?;
        eprintln!("[file-open] dispatch_file_open ext='{ext}' url='{url}'");
        let instance = self
            .extensions
            .get_mut(&ext)
            .ok_or_else(|| format!("files extension '{ext}' not loaded"))?;
        instance
            .file_open(handle, url)
            .map_err(|e| format!("{e:?}"))
    }

    fn dispatch_file_read(
        &mut self,
        file: u32,
        offset: u64,
        len: u32,
    ) -> Result<Vec<u8>, String> {
        let (ext, handle) = self.resolve_files_backend()?;
        let instance = self
            .extensions
            .get_mut(&ext)
            .ok_or_else(|| format!("files extension '{ext}' not loaded"))?;
        instance
            .file_read(handle, file, offset, len)
            .map_err(|e| format!("{e:?}"))
    }

    fn dispatch_file_close(&mut self, file: u32) -> Result<(), String> {
        let (ext, handle) = self.resolve_files_backend()?;
        let instance = self
            .extensions
            .get_mut(&ext)
            .ok_or_else(|| format!("files extension '{ext}' not loaded"))?;
        instance
            .file_close(handle, file)
            .map_err(|e| format!("{e:?}"))
    }

    fn attach_core(&mut self, core: Arc<Mutex<CoreExecution>>) {
        self.core = Some(core);
    }

    /// v1.1 live-query host import: share the CLI's live connection so a
    /// query-capable component's `query` import runs catalog SELECTs on the same
    /// connection the user is on.
    fn attach_current_connection(&mut self, conn: Arc<Mutex<Option<ResourceAny>>>) {
        self.current_connection = conn;
    }

    /// v1.1 live-query host import: the shared catalog snapshot, so the CLI
    /// (`HostState`) refreshes the same snapshot each component's CoreServices
    /// reads when the core is busy.
    fn catalog_snapshot(&self) -> Arc<Mutex<CatalogSnapshot>> {
        self.catalog_snapshot.clone()
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
                .unwrap_or_else(|e| e.into_inner());
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

    #[allow(clippy::ptr_arg)] // forwarded to a bindgen call that takes &Vec (rowbatch)
    fn dispatch_scalar_batch(
        &mut self,
        handle: u32,
        rows: &Vec<Vec<extension_types::Duckvalue>>,
        ctx: extension_runtime::Invokeinfo,
    ) -> Result<Vec<extension_types::Duckvalue>, extension_types::Duckerror> {
        let entry = match self.lookup_callback(handle, CallbackKind::Scalar) {
            Some(entry) => entry,
            None => {
                eprintln!(
                    "[extension-manager] dispatch_scalar_batch received unknown handle {handle}"
                );
                return Err(extension_types::Duckerror::Invalidstate(format!(
                    "unknown scalar callback handle {handle}"
                )));
            }
        };
        let instance = match self.extensions.get_mut(&entry.extension) {
            Some(instance) => instance,
            None => {
                return Err(extension_types::Duckerror::Invalidstate(format!(
                    "extension {} is not loaded",
                    entry.extension
                )));
            }
        };
        instance.dispatch_scalar_batch(entry.dispatcher_handle, rows, ctx)
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

    fn dispatch_cast(
        &mut self,
        handle: u32,
        value: &extension_types::Duckvalue,
    ) -> Result<extension_types::Duckvalue, extension_types::Duckerror> {
        let entry = match self.lookup_callback(handle, CallbackKind::Cast) {
            Some(entry) => entry,
            None => {
                eprintln!("[extension-manager] dispatch_cast received unknown handle {handle}");
                return Err(extension_types::Duckerror::Invalidstate(format!(
                    "unknown cast callback handle {handle}"
                )));
            }
        };
        let instance = match self.extensions.get_mut(&entry.extension) {
            Some(instance) => instance,
            None => {
                return Err(extension_types::Duckerror::Invalidstate(format!(
                    "extension {} is not loaded",
                    entry.extension
                )))
            }
        };
        instance.dispatch_cast(entry.dispatcher_handle, value)
    }

    fn lookup_callback(&self, handle: u32, kind: CallbackKind) -> Option<CallbackEntry> {
        let registry = self
            .callback_registry
            .lock()
            .unwrap_or_else(|e| e.into_inner());
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
        let current_connection = self.current_connection.clone();
        let catalog_snapshot = self.catalog_snapshot.clone();
        let extension_name = sanitized.clone();
        // The shared compose:dynlink provider registry (populated from
        // DUCKLINK_PROVIDERS). Cloned into the load thread; the bridge is built
        // there. A component that imports compose:dynlink/linker (e.g.
        // mlkmeans) resolves the one resident pylon through it; every other
        // extension ignores it (the imports_linker gate in load_component).
        let dynlink_registry = dynlink_provider_registry(&engine).clone();
        // Log the human version AND the authoritative content-addressed contract
        // identity (the witcanon digest, short hex). The digest is what
        // catalog-verify enforces; the version is the runtime-observable proxy.
        let contract_digest = ducklink_runtime::contract_digest();
        eprintln!(
            "[extension-manager] attempting to load '{sanitized}' from {} (host duckdb:extension contract {} digest {})",
            artifact_path.display(),
            ducklink_runtime::ducklink_contract_version(),
            &contract_digest[..contract_digest.len().min(12)]
        );
        // The thread returns the loaded instance AND whether this component
        // imports the live-query capability. Only a query-importing component
        // makes the per-`execute` catalog-snapshot refresh worthwhile; for the
        // 99% of loads that don't (every non-autocomplete extension), the
        // snapshot stays disabled so plain queries pay nothing (see
        // `refresh_catalog_snapshot`'s `enabled` short-circuit).
        let handle = thread::spawn(move || -> wasmtime::Result<(ExtensionInstance, bool)> {
            // Outbound network is a GRANTED capability for extension components,
            // off by default and opt-in via `DUCKLINK_NETWORK_GRANT`. This mirrors
            // how DuckDB function capabilities are declared-then-granted (the
            // registry declares `network` in an extension's `requires`; the host
            // decides whether to honour it). It is best-effort, not a true
            // sandbox: without the grant the WasiCtx simply denies wasi:sockets,
            // so a net-using extension (dns, http) fails to connect rather than
            // being hard-prevented from trying.
            let grant_network = network_grant_allows(&extension_name);
            eprintln!(
                "[extension-manager] '{extension_name}' network capability: {}",
                if grant_network {
                    "GRANTED"
                } else {
                    "denied (opt in with DUCKLINK_NETWORK_GRANT=all|<names>)"
                }
            );
            let mut builder = WasiCtxBuilder::new();
            builder.inherit_env().inherit_stdio();
            if grant_network {
                builder.inherit_network().allow_ip_name_lookup(true);
            }
            let wasi = builder.build();
            let component = Component::from_file(&engine, &artifact_path).map_err(|err| {
                wasmtime::Error::msg(format!(
                    "failed to load component for {extension_name} at {}: {err}",
                    artifact_path.display()
                ))
            })?;
            // Detect whether this component imports the live-query capability
            // (`duckdb:extension/query`) BEFORE instantiating; only those (e.g.
            // autocomplete) need the per-`execute` catalog-snapshot refresh.
            let imports_query = component_imports_query(&engine, &component);
            // The instantiate -> run load() orchestration is the direction-agnostic
            // loader, shared from ducklink-runtime. The host supplies the wasi
            // context (it owns the network-grant policy above) and CoreServices
            // (config/logging routed to DuckDB-compiled-to-wasm).
            ducklink_runtime::load_component_with_dynlink(
                &engine,
                &component,
                wasi,
                Box::new(CoreServices {
                    core,
                    current_connection,
                    catalog_snapshot,
                }),
                callback_registry,
                extension_name.clone(),
                Some(dynlink_registry),
            )
            .map(|instance| (instance, imports_query))
        });

        let (instance, imports_query) = match handle.join() {
            Ok(result) => match result {
                Ok(pair) => pair,
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
        // PERF GATE: enable the CLI-boundary catalog-snapshot refresh ONLY when a
        // query-importing component is loaded (the re-entrancy fallback that lets
        // catalog completion answer from inside a query). Loads that don't import
        // `query` leave the snapshot disabled, so plain queries skip the refresh.
        if imports_query {
            eprintln!(
                "[extension-manager] '{loaded_name}' imports the live-query capability; \
                 enabling catalog-snapshot refresh",
                loaded_name = sanitized
            );
            self.catalog_snapshot
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .enabled = true;
        }
        let loaded_name = sanitized.clone();
        self.extensions.insert(sanitized, instance);
        // v1.1: collation/pragma defs taken inside the `instance` borrow below,
        // then handed to `prefix_collations`/`prefix_pragmas` AFTER the borrow
        // ends (those methods borrow `self` whole).
        let mut collations_captured: Vec<reg::CollationReg> = Vec::new();
        let mut pragmas_captured: Vec<reg::PragmaReg> = Vec::new();
        // M2a: capture this extension's storage backends NOW (right after load),
        // so an `ATTACH ... (TYPE <name>)` can route to it without waiting for the
        // core's function-registration drain. Only the storage registrations are
        // taken; scalars/tables/... stay pending for the normal hook flow.
        if let Some(instance) = self.extensions.get_mut(&loaded_name) {
            for storage in instance.take_pending_storages() {
                eprintln!(
                    "[extension-manager] storage backend '{}' -> extension '{}' (callback={})",
                    storage.type_name, storage.extension, storage.callback_handle
                );
                self.storage_backends.insert(
                    storage.type_name.clone(),
                    (storage.extension.clone(), storage.callback_handle),
                );
            }
            // Item 3 / M2a: capture this extension's custom index TYPEs NOW (right
            // after load), so the core can register them (via
            // index-host.index-type-list) before the first CREATE INDEX.
            for index in instance.take_pending_indexes() {
                eprintln!(
                    "[extension-manager] index type '{}' -> extension '{}'",
                    index.type_name, index.extension
                );
                self.index_backends
                    .insert(index.type_name.clone(), index.extension.clone());
            }
            // httpfs M2: capture this extension's files backend NOW (right after
            // load), so an http(s):// read can route to it. The last loaded
            // files backend wins.
            for files in instance.take_pending_files() {
                eprintln!(
                    "[extension-manager] files backend -> extension '{}' (callback={})",
                    files.extension, files.callback_handle
                );
                self.files_backend = Some((files.extension.clone(), files.callback_handle));
            }
            // Item 2: capture this extension's collations NOW (right after load),
            // so the core can register them (via collation-host.collation-list)
            // before the first query that uses `COLLATE <name>`.
            collations_captured = instance.take_pending_collations();
            pragmas_captured = instance.take_pending_pragmas();
            for collation in &collations_captured {
                eprintln!(
                    "[extension-manager] collation '{}' -> extension '{}' (transform scalar='{}', combinable={})",
                    collation.name, collation.extension, collation.transform_scalar, collation.combinable
                );
                self.collations.insert(
                    collation.name.clone(),
                    (collation.transform_scalar.clone(), collation.combinable),
                );
            }
            // Item 4: capture this extension's pragmas NOW (right after load), so
            // the core can intercept `PRAGMA <name>(...)` (via pragma-host.pragma-list)
            // before the first query that uses it.
            for pragma in &pragmas_captured {
                eprintln!(
                    "[extension-manager] pragma '{}' -> extension '{}' (callback={})",
                    pragma.name, pragma.extension, pragma.callback_handle
                );
                self.pragmas.insert(
                    pragma.name.clone(),
                    (pragma.extension.clone(), pragma.callback_handle),
                );
            }
        }
        // v1.1: collation/pragma are dispatched by NAME, so `{prefix}__{name}`
        // namespacing applies identically — register the qualified form into the
        // same map (the core pulls it by name), track cross-component collisions,
        // stage __ducklink_prefix_function rows, and retain the bare def. Run
        // OUTSIDE the `instance` borrow above (these methods borrow `self` whole).
        // Collation arity = 0 (no call args); pragma arity = -1 (variadic).
        self.prefix_collations(&collations_captured);
        self.prefix_pragmas(&pragmas_captured);
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
        // M2a: capture storage backends so ATTACH (TYPE ...) can route to the
        // backing component. The core hooks don't carry storages, so record the
        // type-name -> (extension, callback-handle) mapping here before the
        // PendingRegistrationsData is converted (and `storages` dropped).
        for storage in &aggregated.storages {
            eprintln!(
                "[extension-manager] storage backend '{}' -> extension '{}' (callback={})",
                storage.type_name, storage.extension, storage.callback_handle
            );
            self.storage_backends.insert(
                storage.type_name.clone(),
                (storage.extension.clone(), storage.callback_handle),
            );
        }
        // PLAN-prefixes: namespace every scalar/table/aggregate registration as
        // `prefix__name` (additive — bare name keeps working), warn on
        // cross-component bare-name collisions, and stage the per-db
        // __ducklink_prefix_function rows. Pure (no SQL / no core re-entry); the
        // staged rows are flushed lazily on the live connection.
        self.apply_function_prefixes(&mut aggregated);
        // v1.1 THE PIN — no mid-drain honor pass is needed: a pin is effected by
        // a wrapper macro (`apply_pins`) that shadows ANY later bare-scalar
        // re-registration, so load-order independence is automatic. The pinned
        // owner keeps winning the bare name even after this fresh load.

        let scalar_names =
            summarize_registration_names(&aggregated.scalars, |entry| entry.name.as_str());
        let table_names =
            summarize_registration_names(&aggregated.tables, |entry| entry.name.as_str());
        let aggregate_names =
            summarize_registration_names(&aggregated.aggregates, |entry| entry.name.as_str());
        let macro_names =
            summarize_registration_names(&aggregated.macros, |entry| entry.name.as_str());
        eprintln!(
            "[extension-manager] aggregated pending registrations: scalars={} ({scalar_names}), tables={} ({table_names}), aggregates={} ({aggregate_names}), macros={} ({macro_names})",
            aggregated.scalars.len(),
            aggregated.tables.len(),
            aggregated.aggregates.len(),
            aggregated.macros.len()
        );
        aggregated
    }
}
// ExtensionStoreState, its pending-registry buffers, PendingRegistrationsData,
// summarize_registration_names, and the ExtensionStoreState capability `Host*`
// impls now live in `ducklink-runtime` (imported above). The host retains only
// the Direction-1 sinks: CoreServices (config/logging) and convert_pending_*
// (registration forwarding into the wasm DuckDB core).

// The pending-registration records are the neutral capture model, defined in
// ducklink-runtime so both directions (wasm-DuckDB host, native-DuckDB
// extension) share one representation. Capture converts the extension's WIT
// types into these; each direction's sink converts these into its own loader
// types. See `convert_extension_*` (capture) and `convert_pending_*` (sink).
use ducklink_runtime::reg;
type PendingScalar = reg::ScalarReg;
type PendingTable = reg::TableReg;
type PendingAggregate = reg::AggregateReg;
type PendingMacro = reg::MacroReg;
type PendingReplacementScan = reg::ReplacementScanReg;
type PendingLogicalType = reg::LogicalTypeReg;
type PendingCast = reg::CastReg;

pub struct HostState {
    table: ResourceTable,
    wasi: WasiCtx,
    core: Arc<Mutex<CoreExecution>>,
    extension_manager: Arc<Mutex<ExtensionManager>>,
    dotcmd_registry: Arc<Mutex<DotcmdRegistry>>,
    /// The CLI's live connection handle, shared with dot-command components' spi.
    current_connection: Arc<Mutex<Option<ResourceAny>>>,
    next_resource_id: u32,
    connections: HashMap<u32, ConnectionEntry>,
    streams: HashMap<u32, StreamEntry>,
    prepared: HashMap<u32, PreparedEntry>,
    appenders: HashMap<u32, AppenderEntry>,
    pending_connection_drops: Vec<Resource<cli_db::Connection>>,
    pending_stream_drops: Vec<Resource<cli_db::ResultStream>>,
    pending_prepared_drops: Vec<Resource<cli_db::PreparedStatement>>,
    pending_appender_drops: Vec<Resource<cli_db::Appender>>,
    /// One-shot guard: the DUCKLINK_AUTOLOAD extensions are loaded once, right
    /// after the first connection opens (the database now exists).
    did_autoload: bool,
    /// v1.1 live-query host import: the re-entrancy fallback catalog snapshot,
    /// refreshed after each `execute` (core idle) so a query-capable component's
    /// `query` import can answer duckdb_tables()/duckdb_columns() even when called
    /// from inside a query.
    catalog_snapshot: Arc<Mutex<CatalogSnapshot>>,
    /// host->guest preopen mapping, used by the `delta_scan('dir')` SQL rewrite
    /// to read a Delta table's `_delta_log` off the real host filesystem.
    preopens: Vec<(PathBuf, String)>,
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

    /// v1.1 live-query re-entrancy fallback: while a query-capable extension is
    /// loaded, re-run the catalog SELECTs a completer asks for (table + column
    /// names) on the now-idle core and cache the rows. Cheap + best-effort: any
    /// error just leaves the previous snapshot in place. Called after each CLI
    /// `execute`, so the snapshot reflects the catalog as of the statement that
    /// just completed -- which is exactly what a subsequent `sql_complete(...)`
    /// (running INSIDE its own query, when the core is busy) needs.
    fn refresh_catalog_snapshot(&self) {
        const CATALOG_QUERIES: &[&str] = &[
            "SELECT table_name FROM duckdb_tables()",
            "SELECT DISTINCT column_name FROM duckdb_columns()",
        ];
        {
            // Poison-tolerant: a snapshot refresh must never abort the query that
            // just completed, so recover the guard rather than panicking.
            let snap = self
                .catalog_snapshot
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            if !snap.enabled {
                return;
            }
        }
        for sql in CATALOG_QUERIES {
            match run_query_on_core(
                self.core.lock().unwrap_or_else(|e| e.into_inner()),
                &self.current_connection,
                sql,
            ) {
                Ok(rows) => {
                    self.catalog_snapshot
                        .lock()
                        .unwrap_or_else(|e| e.into_inner())
                        .rows
                        .insert((*sql).to_string(), rows);
                }
                Err(_) => { /* keep the previous snapshot for this SQL */ }
            }
        }
    }

    /// Load the components named in DUCKLINK_AUTOLOAD (comma/space separated)
    /// exactly once, right after the first connection opens. Deployments running
    /// the lean core (no embedded json/etc.) set this to the replacement
    /// components, e.g. DUCKLINK_AUTOLOAD=jsonfns. Best-effort: a failure (e.g.
    /// a name colliding with a still-embedded function on a fat core) is logged
    /// and skipped rather than aborting startup.
    fn maybe_autoload(&mut self) {
        if self.did_autoload {
            return;
        }
        self.did_autoload = true;
        // The default core is lean (no embedded official extensions), so json --
        // the one functional gap in the suite -- is provided by the `jsonfns`
        // component, auto-loaded by default. Override with DUCKLINK_AUTOLOAD
        // (set it empty to disable, or to a different/longer list). On a fat core
        // the jsonfns LOAD collides with embedded json and is skipped harmlessly.
        let spec = std::env::var("DUCKLINK_AUTOLOAD").unwrap_or_else(|_| String::from("jsonfns"));
        // Run `LOAD <name>` as SQL on the freshly-opened connection so the core's
        // normal load orchestration applies the component's registrations to the
        // connection (calling ensure_extension_loaded directly only buffers them).
        let handle = match self
            .current_connection
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
        {
            Some(h) => h,
            None => return,
        };
        for name in spec.split(|c: char| c == ',' || c.is_whitespace()) {
            let name = name.trim();
            if name.is_empty() {
                continue;
            }
            if !name
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
            {
                eprintln!("[autoload] skipped invalid extension name '{name}'");
                continue;
            }
            let sql = format!("LOAD {name};");
            let res = self.with_core(|core| {
                core.with_database(|guest, store| guest.call_execute(store, handle.clone(), &sql))
            });
            match res {
                Ok(Ok(_)) => eprintln!("[autoload] loaded '{name}'"),
                Ok(Err(err)) => {
                    eprintln!("[autoload] skipped '{name}': {}", core_duckerror_message(err))
                }
                Err(trap) => eprintln!("[autoload] skipped '{name}': {trap}"),
            }
        }
    }

    fn with_core<F, R>(&self, f: F) -> R
    where
        F: FnOnce(&mut CoreExecution) -> R,
    {
        let mut core = self.core.lock().unwrap_or_else(|e| e.into_inner());
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
        let pending_prepared = std::mem::take(&mut self.pending_prepared_drops);
        for prepared in pending_prepared {
            self.drop_prepared_resource(prepared)?;
        }
        let pending_appenders = std::mem::take(&mut self.pending_appender_drops);
        for appender in pending_appenders {
            self.drop_appender_resource(appender)?;
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

    fn drop_prepared_resource(
        &mut self,
        rep: Resource<cli_db::PreparedStatement>,
    ) -> Result<(), cli_types::Duckerror> {
        if let Some(entry) = self.prepared.remove(&rep.rep()) {
            self.with_core(|core| {
                core.with_prepared(|_guest, store| entry.handle.resource_drop(store))
            })
            .map_err(|err| cli_types::Duckerror::Internal(trap_to_cli_string(err)))?;
        }
        Ok(())
    }

    fn schedule_prepared_drop(&mut self, prepared: Resource<cli_db::PreparedStatement>) {
        self.pending_prepared_drops.push(prepared);
    }

    fn drop_appender_resource(
        &mut self,
        rep: Resource<cli_db::Appender>,
    ) -> Result<(), cli_types::Duckerror> {
        if let Some(entry) = self.appenders.remove(&rep.rep()) {
            self.with_core(|core| {
                core.with_appender(|_guest, store| entry.handle.resource_drop(store))
            })
            .map_err(|err| cli_types::Duckerror::Internal(trap_to_cli_string(err)))?;
        }
        Ok(())
    }

    fn schedule_appender_drop(&mut self, appender: Resource<cli_db::Appender>) {
        self.pending_appender_drops.push(appender);
    }
}

// Retained only for the test mocks below (the production impls moved to
// ducklink-runtime).
#[cfg(test)]
fn unsupported_runtime_error() -> extension_types::Duckerror {
    extension_types::Duckerror::Unsupported(
        "component runtime not available in CLI host".to_string(),
    )
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
        // `schema` returns a plain Vec (no error channel), so a bad/closed
        // handle or a core trap degrades to an empty schema rather than
        // aborting the host from inside this trait impl.
        let handle = match self.streams.get(&rep.rep()) {
            Some(entry) => entry.handle.clone(),
            None => {
                eprintln!("[host] schema() for unknown stream handle {}", rep.rep());
                return Vec::new().into();
            }
        };
        let columns = match self
            .with_core(|core| core.with_stream(|guest, store| guest.call_schema(store, handle)))
        {
            Ok(columns) => columns,
            Err(err) => {
                eprintln!("[host] schema() failed to fetch stream schema: {err}");
                return Vec::new().into();
            }
        };
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
            // `close` has no error channel; a trap here must not abort the host
            // from inside this trait impl. Log and mark the stream closed anyway.
            eprintln!("[host] close() failed to close result stream: {err}");
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

impl cli_db::HostPreparedStatement for HostState {
    fn parameter_count(&mut self, rep: Resource<cli_db::PreparedStatement>) -> u32 {
        let handle = match self.prepared.get(&rep.rep()) {
            Some(entry) => entry.handle.clone(),
            None => return 0,
        };
        self.with_core(|core| {
            core.with_prepared(|guest, store| guest.call_parameter_count(store, handle))
        })
        .expect("failed to fetch prepared-statement parameter count")
    }

    fn execute(
        &mut self,
        rep: Resource<cli_db::PreparedStatement>,
        params: wasmtime::component::__internal::Vec<cli_types::Duckvalue>,
    ) -> Result<cli_db::QueryResult, cli_types::Duckerror> {
        let handle = self
            .prepared
            .get(&rep.rep())
            .ok_or_else(|| cli_types::Duckerror::Internal("unknown prepared statement".into()))?
            .handle
            .clone();
        let core_params: Vec<core_types::Duckvalue> =
            params.into_iter().map(convert_cli_duckvalue).collect();
        let result = self
            .with_core(|core| {
                core.with_prepared(|guest, store| {
                    guest.call_execute(store, handle, &core_params)
                })
            })
            .map_err(convert_trap_to_duckerror)?;
        match result {
            Ok(value) => Ok(convert_core_query_result(value)),
            Err(err) => Err(convert_core_duckerror(err)),
        }
    }

    fn drop(&mut self, rep: Resource<cli_db::PreparedStatement>) -> wasmtime::Result<()> {
        self.schedule_prepared_drop(rep);
        Ok(())
    }
}

impl cli_db::HostAppender for HostState {
    fn append_row(
        &mut self,
        rep: Resource<cli_db::Appender>,
        values: wasmtime::component::__internal::Vec<cli_types::Duckvalue>,
    ) -> Result<(), cli_types::Duckerror> {
        let handle = self
            .appenders
            .get(&rep.rep())
            .ok_or_else(|| cli_types::Duckerror::Internal("unknown appender".into()))?
            .handle
            .clone();
        let core_values: Vec<core_types::Duckvalue> =
            values.into_iter().map(convert_cli_duckvalue).collect();
        self.with_core(|core| {
            core.with_appender(|guest, store| guest.call_append_row(store, handle, &core_values))
        })
        .map_err(convert_trap_to_duckerror)?
        .map_err(convert_core_duckerror)
    }

    fn flush(&mut self, rep: Resource<cli_db::Appender>) -> Result<(), cli_types::Duckerror> {
        let handle = self
            .appenders
            .get(&rep.rep())
            .ok_or_else(|| cli_types::Duckerror::Internal("unknown appender".into()))?
            .handle
            .clone();
        self.with_core(|core| core.with_appender(|guest, store| guest.call_flush(store, handle)))
            .map_err(convert_trap_to_duckerror)?
            .map_err(convert_core_duckerror)
    }

    fn close(&mut self, rep: Resource<cli_db::Appender>) -> Result<(), cli_types::Duckerror> {
        let handle = self
            .appenders
            .get(&rep.rep())
            .ok_or_else(|| cli_types::Duckerror::Internal("unknown appender".into()))?
            .handle
            .clone();
        self.with_core(|core| core.with_appender(|guest, store| guest.call_close(store, handle)))
            .map_err(convert_trap_to_duckerror)?
            .map_err(convert_core_duckerror)
    }

    fn drop(&mut self, rep: Resource<cli_db::Appender>) -> wasmtime::Result<()> {
        self.schedule_appender_drop(rep);
        Ok(())
    }
}

impl cli_db::Host for HostState {
    /// The UI server drives the core's `handle-ui-request` directly (see
    /// `ui_server.rs`); the CLI shell never serves UI through its connection, so
    /// this host-side database function is a no-op for the CLI.
    fn handle_ui_request(
        &mut self,
        _method: CliString,
        _path: CliString,
        _headers: CliString,
        _body: wasmtime::component::__internal::Vec<u8>,
    ) -> Option<cli_db::UiResponse> {
        None
    }

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
                // Track the CLI's live connection so dot-command components' spi
                // runs SQL on the same connection (shared temp tables / state).
                *self
                    .current_connection
                    .lock()
                    .unwrap_or_else(|e| e.into_inner()) = Some(handle.clone());
                self.connections.insert(
                    id,
                    ConnectionEntry {
                        handle,
                        closed: false,
                    },
                );
                self.maybe_autoload();
                Ok(Resource::new_own(id))
            }
            Err(err) => Err(err),
        }
    }

    fn open_with_config(
        &mut self,
        path: Option<CliString>,
        options: wasmtime::component::__internal::Vec<(CliString, CliString)>,
    ) -> Result<Resource<cli_db::Connection>, CliString> {
        let owned_path: Option<String> = path.map(|s| s.into());
        let owned_options: Vec<(String, String)> = options
            .into_iter()
            .map(|(name, value)| (name.into(), value.into()))
            .collect();
        let result = self
            .with_core(|core| {
                core.with_database(|guest, store| {
                    guest.call_open_with_config(store, owned_path.as_deref(), &owned_options)
                })
            })
            .map_err(trap_to_cli_string)?;
        match result {
            Ok(handle) => {
                let id = self.alloc_resource_id();
                // Track the CLI's live connection so dot-command components' spi
                // runs SQL on the same connection (shared temp tables / state).
                *self
                    .current_connection
                    .lock()
                    .unwrap_or_else(|e| e.into_inner()) = Some(handle.clone());
                self.connections.insert(
                    id,
                    ConnectionEntry {
                        handle,
                        closed: false,
                    },
                );
                self.maybe_autoload();
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
        // One-query `delta_scan('dir')`: the wasm core can't take a subquery-
        // valued table-fn arg, so the host reads the table's _delta_log off the
        // real filesystem, resolves the active files (add minus remove), and
        // rewrites the call to a read_parquet([...]) the core can scan. No-op
        // when the SQL has no rewritable delta_scan call.
        let sql = delta_rewrite::rewrite_delta_scan(&sql, &self.preopens);
        let result = self
            .with_core(|core| {
                core.with_database(|guest, store| {
                    guest.call_execute(store, entry.handle.clone(), &sql)
                })
            })
            .map_err(convert_trap_to_duckerror)?;
        // v1.1: the core is idle again here -> refresh the catalog snapshot so a
        // query-capable component's `query` import (which runs INSIDE a later
        // query, when the core is busy) can still answer catalog SELECTs.
        self.refresh_catalog_snapshot();
        match result {
            Ok(value) => Ok(convert_core_query_result(value)),
            Err(err) => Err(convert_core_duckerror(err)),
        }
    }

    fn query_arrow(
        &mut self,
        conn: Resource<cli_db::Connection>,
        sql: CliString,
    ) -> Result<wasmtime::component::__internal::Vec<u8>, cli_types::Duckerror> {
        let entry = self
            .connections
            .get(&conn.rep())
            .ok_or_else(|| cli_types::Duckerror::Internal("unknown connection".into()))?;
        let result = self
            .with_core(|core| {
                core.with_database(|guest, store| {
                    guest.call_query_arrow(store, entry.handle.clone(), &sql)
                })
            })
            .map_err(convert_trap_to_duckerror)?;
        match result {
            Ok(bytes) => Ok(bytes.into()),
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

    fn prepare(
        &mut self,
        conn: Resource<cli_db::Connection>,
        sql: CliString,
    ) -> Result<Resource<cli_db::PreparedStatement>, cli_types::Duckerror> {
        let entry = self
            .connections
            .get(&conn.rep())
            .ok_or_else(|| cli_types::Duckerror::Internal("unknown connection".into()))?;
        let prepared = self
            .with_core(|core| {
                core.with_database(|guest, store| {
                    guest.call_prepare(store, entry.handle.clone(), &sql)
                })
            })
            .map_err(convert_trap_to_duckerror)?;
        match prepared {
            Ok(handle) => {
                let id = self.alloc_resource_id();
                self.prepared.insert(id, PreparedEntry { handle });
                Ok(Resource::new_own(id))
            }
            Err(err) => Err(convert_core_duckerror(err)),
        }
    }

    fn create_appender(
        &mut self,
        conn: Resource<cli_db::Connection>,
        schema: Option<CliString>,
        table: CliString,
    ) -> Result<Resource<cli_db::Appender>, cli_types::Duckerror> {
        let handle = self
            .connections
            .get(&conn.rep())
            .ok_or_else(|| cli_types::Duckerror::Internal("unknown connection".into()))?
            .handle
            .clone();
        let owned_schema: Option<String> = schema.map(|s| s.into());
        let owned_table: String = table.into();
        let appender = self
            .with_core(|core| {
                core.with_database(|guest, store| {
                    guest.call_create_appender(
                        store,
                        handle,
                        owned_schema.as_deref(),
                        &owned_table,
                    )
                })
            })
            .map_err(convert_trap_to_duckerror)?;
        match appender {
            Ok(handle) => {
                let id = self.alloc_resource_id();
                self.appenders.insert(id, AppenderEntry { handle });
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
            "[ducklink] register_extension requested: name='{extension_name}', capabilities={capability_summary}"
        );
        let result = match self.with_core(|core| {
            core.with_database(|guest, store| {
                guest.call_register_extension(store, &name, capability_list.as_slice())
            })
        }) {
            Ok(result) => result,
            Err(err) => {
                eprintln!(
                    "[ducklink] failed to invoke core register_extension for '{extension_name}': {err}"
                );
                return Err(trap_to_cli_string(err));
            }
        };
        match result {
            Ok(value) => {
                eprintln!(
                    "[ducklink] core register_extension completed for '{extension_name}' (registered={value})"
                );
                Ok(value)
            }
            Err(err) => {
                let err_msg: String = err.clone().into();
                eprintln!(
                    "[ducklink] core register_extension rejected '{extension_name}': {err_msg}"
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
        macros: data
            .macros
            .into_iter()
            .map(convert_pending_macro_registration)
            .collect::<Vec<_>>()
            .into(),
        replacement_scans: data
            .replacement_scans
            .into_iter()
            .map(convert_pending_replacement_scan_registration)
            .collect::<Vec<_>>()
            .into(),
        logical_types: data
            .logical_types
            .into_iter()
            .map(convert_pending_logical_type_registration)
            .collect::<Vec<_>>()
            .into(),
        casts: data
            .casts
            .into_iter()
            .map(convert_pending_cast_registration)
            .collect::<Vec<_>>()
            .into(),
    }
}

fn convert_pending_logical_type_registration(
    entry: PendingLogicalType,
) -> core_extension_hooks::LogicalTypeRegistration {
    core_extension_hooks::LogicalTypeRegistration {
        name: entry.name,
        physical: entry.physical,
    }
}

fn convert_pending_cast_registration(
    entry: PendingCast,
) -> core_extension_hooks::CastRegistration {
    core_extension_hooks::CastRegistration {
        source: entry.source,
        target: entry.target,
        callback_handle: entry.callback_handle,
    }
}

fn convert_pending_macro_registration(
    entry: PendingMacro,
) -> core_extension_hooks::MacroRegistration {
    core_extension_hooks::MacroRegistration {
        schema: entry.schema,
        name: entry.name,
        parameters: entry.parameters.into(),
        definition_sql: entry.definition_sql,
    }
}

fn convert_pending_replacement_scan_registration(
    entry: PendingReplacementScan,
) -> core_extension_hooks::ReplacementScanRegistration {
    core_extension_hooks::ReplacementScanRegistration {
        extensions: entry.extensions.into(),
        function_name: entry.function_name,
    }
}

fn convert_pending_scalar_registration(
    entry: PendingScalar,
) -> core_extension_hooks::ScalarRegistration {
    log_pending_scalar_conversion(&entry);
    core_extension_hooks::ScalarRegistration {
        name: entry.name,
        arguments: convert_funcargs_to_loader(entry.arguments),
        returns: neutral_logicaltype_to_core(entry.returns),
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
        columns: entry
            .columns
            .into_iter()
            .map(neutral_columndef_to_core)
            .collect::<Vec<_>>()
            .into(),
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
        returns: neutral_logicaltype_to_core(entry.returns),
        callback_handle: entry.callback_handle,
        options: entry.options.map(convert_funcopts_to_loader),
    }
}

// Direction-1 sink: neutral `reg::*` capture records -> wasm-DuckDB-core loader
// types. (Direction 2, the native extension, will provide its own sink against
// the DuckDB C API.)
fn neutral_logicaltype_to_core(ty: reg::LogicalType) -> core_runtime_exports::Logicaltype {
    match ty {
        reg::LogicalType::Boolean => core_runtime_exports::Logicaltype::Boolean,
        reg::LogicalType::Int64 => core_runtime_exports::Logicaltype::Int64,
        reg::LogicalType::Uint64 => core_runtime_exports::Logicaltype::Uint64,
        reg::LogicalType::Float64 => core_runtime_exports::Logicaltype::Float64,
        reg::LogicalType::Text => core_runtime_exports::Logicaltype::Text,
        reg::LogicalType::Blob => core_runtime_exports::Logicaltype::Blob,
        reg::LogicalType::Int32 => core_runtime_exports::Logicaltype::Int32,
        reg::LogicalType::Timestamp => core_runtime_exports::Logicaltype::Timestamp,
        reg::LogicalType::Int8 => core_runtime_exports::Logicaltype::Int8,
        reg::LogicalType::Int16 => core_runtime_exports::Logicaltype::Int16,
        reg::LogicalType::Uint8 => core_runtime_exports::Logicaltype::Uint8,
        reg::LogicalType::Uint16 => core_runtime_exports::Logicaltype::Uint16,
        reg::LogicalType::Uint32 => core_runtime_exports::Logicaltype::Uint32,
        reg::LogicalType::Float32 => core_runtime_exports::Logicaltype::Float32,
        reg::LogicalType::Date => core_runtime_exports::Logicaltype::Date,
        reg::LogicalType::Time => core_runtime_exports::Logicaltype::Time,
        reg::LogicalType::Timestamptz => core_runtime_exports::Logicaltype::Timestamptz,
        reg::LogicalType::Decimal => core_runtime_exports::Logicaltype::Decimal,
        reg::LogicalType::Interval => core_runtime_exports::Logicaltype::Interval,
        reg::LogicalType::Uuid => core_runtime_exports::Logicaltype::Uuid,
        reg::LogicalType::Complex(expr) => core_runtime_exports::Logicaltype::Complex(expr),
    }
}

fn neutral_funcflags_to_core(flags: reg::FuncFlags) -> core_types::Funcflags {
    let mut result = core_types::Funcflags::empty();
    if flags.deterministic {
        result |= core_types::Funcflags::DETERMINISTIC;
    }
    if flags.commutative {
        result |= core_types::Funcflags::COMMUTATIVE;
    }
    if flags.stateless {
        result |= core_types::Funcflags::STATELESS;
    }
    if flags.side_effecting {
        result |= core_types::Funcflags::SIDEEFFECTING;
    }
    if flags.deprecated {
        result |= core_types::Funcflags::DEPRECATED;
    }
    result
}

fn neutral_columndef_to_core(col: reg::ColumnDef) -> core_runtime_exports::Columndef {
    core_runtime_exports::Columndef {
        name: col.name,
        logical: neutral_logicaltype_to_core(col.logical),
    }
}

fn convert_funcargs_to_loader(args: Vec<reg::FuncArg>) -> BindgenVec<core_extension_hooks::FuncArg> {
    args.into_iter()
        .map(|arg| core_extension_hooks::FuncArg {
            name: arg.name,
            logical: neutral_logicaltype_to_core(arg.logical),
        })
        .collect::<Vec<_>>()
        .into()
}

fn convert_funcopts_to_loader(opts: reg::FuncOpts) -> core_extension_hooks::FuncOpts {
    core_extension_hooks::FuncOpts {
        description: opts.description,
        tags: opts.tags.into_iter().collect::<Vec<_>>().into(),
        attributes: neutral_funcflags_to_core(opts.attributes),
    }
}

fn convert_extopts_to_loader(opts: reg::ExtOpts) -> core_extension_hooks::ExtOpts {
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
        core_types::Duckvalue::Int32(v) => cli_types::Duckvalue::Int32(v),
        core_types::Duckvalue::Timestamp(v) => cli_types::Duckvalue::Timestamp(v),
        core_types::Duckvalue::Int8(v) => cli_types::Duckvalue::Int8(v),
        core_types::Duckvalue::Int16(v) => cli_types::Duckvalue::Int16(v),
        core_types::Duckvalue::Uint8(v) => cli_types::Duckvalue::Uint8(v),
        core_types::Duckvalue::Uint16(v) => cli_types::Duckvalue::Uint16(v),
        core_types::Duckvalue::Uint32(v) => cli_types::Duckvalue::Uint32(v),
        core_types::Duckvalue::Float32(v) => cli_types::Duckvalue::Float32(v),
        core_types::Duckvalue::Date(v) => cli_types::Duckvalue::Date(v),
        core_types::Duckvalue::Time(v) => cli_types::Duckvalue::Time(v),
        core_types::Duckvalue::Timestamptz(v) => cli_types::Duckvalue::Timestamptz(v),
        core_types::Duckvalue::Decimal(d) => cli_types::Duckvalue::Decimal(cli_types::Decimalvalue {
            lower: d.lower,
            upper: d.upper,
            width: d.width,
            scale: d.scale,
        }),
        core_types::Duckvalue::Interval(iv) => {
            cli_types::Duckvalue::Interval(cli_types::Intervalvalue {
                months: iv.months,
                days: iv.days,
                micros: iv.micros,
            })
        }
        core_types::Duckvalue::Uuid(u) => {
            cli_types::Duckvalue::Uuid(cli_types::Uuidvalue { hi: u.hi, lo: u.lo })
        }
        core_types::Duckvalue::Complex(c) => {
            cli_types::Duckvalue::Complex(cli_types::Complexvalue {
                type_expr: c.type_expr,
                json: c.json,
            })
        }
    }
}

fn convert_cli_duckvalue(value: cli_types::Duckvalue) -> core_types::Duckvalue {
    match value {
        cli_types::Duckvalue::Null => core_types::Duckvalue::Null,
        cli_types::Duckvalue::Boolean(v) => core_types::Duckvalue::Boolean(v),
        cli_types::Duckvalue::Int64(v) => core_types::Duckvalue::Int64(v),
        cli_types::Duckvalue::Uint64(v) => core_types::Duckvalue::Uint64(v),
        cli_types::Duckvalue::Float64(v) => core_types::Duckvalue::Float64(v),
        cli_types::Duckvalue::Text(v) => core_types::Duckvalue::Text(v.into()),
        cli_types::Duckvalue::Blob(v) => core_types::Duckvalue::Blob(v.into()),
        cli_types::Duckvalue::Int32(v) => core_types::Duckvalue::Int32(v),
        cli_types::Duckvalue::Timestamp(v) => core_types::Duckvalue::Timestamp(v),
        cli_types::Duckvalue::Int8(v) => core_types::Duckvalue::Int8(v),
        cli_types::Duckvalue::Int16(v) => core_types::Duckvalue::Int16(v),
        cli_types::Duckvalue::Uint8(v) => core_types::Duckvalue::Uint8(v),
        cli_types::Duckvalue::Uint16(v) => core_types::Duckvalue::Uint16(v),
        cli_types::Duckvalue::Uint32(v) => core_types::Duckvalue::Uint32(v),
        cli_types::Duckvalue::Float32(v) => core_types::Duckvalue::Float32(v),
        cli_types::Duckvalue::Date(v) => core_types::Duckvalue::Date(v),
        cli_types::Duckvalue::Time(v) => core_types::Duckvalue::Time(v),
        cli_types::Duckvalue::Timestamptz(v) => core_types::Duckvalue::Timestamptz(v),
        cli_types::Duckvalue::Decimal(d) => {
            core_types::Duckvalue::Decimal(core_types::Decimalvalue {
                lower: d.lower,
                upper: d.upper,
                width: d.width,
                scale: d.scale,
            })
        }
        cli_types::Duckvalue::Interval(iv) => {
            core_types::Duckvalue::Interval(core_types::Intervalvalue {
                months: iv.months,
                days: iv.days,
                micros: iv.micros,
            })
        }
        cli_types::Duckvalue::Uuid(u) => {
            core_types::Duckvalue::Uuid(core_types::Uuidvalue { hi: u.hi, lo: u.lo })
        }
        cli_types::Duckvalue::Complex(c) => {
            core_types::Duckvalue::Complex(core_types::Complexvalue {
                type_expr: c.type_expr,
                json: c.json,
            })
        }
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
        core_types::Logicaltype::Int32 => cli_types::Logicaltype::Int32,
        core_types::Logicaltype::Timestamp => cli_types::Logicaltype::Timestamp,
        core_types::Logicaltype::Int8 => cli_types::Logicaltype::Int8,
        core_types::Logicaltype::Int16 => cli_types::Logicaltype::Int16,
        core_types::Logicaltype::Uint8 => cli_types::Logicaltype::Uint8,
        core_types::Logicaltype::Uint16 => cli_types::Logicaltype::Uint16,
        core_types::Logicaltype::Uint32 => cli_types::Logicaltype::Uint32,
        core_types::Logicaltype::Float32 => cli_types::Logicaltype::Float32,
        core_types::Logicaltype::Date => cli_types::Logicaltype::Date,
        core_types::Logicaltype::Time => cli_types::Logicaltype::Time,
        core_types::Logicaltype::Timestamptz => cli_types::Logicaltype::Timestamptz,
        core_types::Logicaltype::Decimal => cli_types::Logicaltype::Decimal,
        core_types::Logicaltype::Interval => cli_types::Logicaltype::Interval,
        core_types::Logicaltype::Uuid => cli_types::Logicaltype::Uuid,
        core_types::Logicaltype::Complex(expr) => cli_types::Logicaltype::Complex(expr),
    }
}

fn convert_core_capabilitykind(kind: core_types::Capabilitykind) -> cli_types::Capabilitykind {
    match kind {
        core_types::Capabilitykind::Scalar => cli_types::Capabilitykind::Scalar,
        core_types::Capabilitykind::Table => cli_types::Capabilitykind::Table,
        core_types::Capabilitykind::Aggregate => cli_types::Capabilitykind::Aggregate,
        core_types::Capabilitykind::Pragma => cli_types::Capabilitykind::Pragma,
        core_types::Capabilitykind::Macro => cli_types::Capabilitykind::Macro,
        core_types::Capabilitykind::Catalog => cli_types::Capabilitykind::Catalog,
        core_types::Capabilitykind::FileFormat => cli_types::Capabilitykind::FileFormat,
    }
}

fn convert_cli_capability(kind: cli_types::Capabilitykind) -> core_types::Capabilitykind {
    match kind {
        cli_types::Capabilitykind::Scalar => core_types::Capabilitykind::Scalar,
        cli_types::Capabilitykind::Table => core_types::Capabilitykind::Table,
        cli_types::Capabilitykind::Aggregate => core_types::Capabilitykind::Aggregate,
        cli_types::Capabilitykind::Pragma => core_types::Capabilitykind::Pragma,
        cli_types::Capabilitykind::Macro => core_types::Capabilitykind::Macro,
        cli_types::Capabilitykind::Catalog => core_types::Capabilitykind::Catalog,
        cli_types::Capabilitykind::FileFormat => core_types::Capabilitykind::FileFormat,
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
        cli_types::Capabilitykind::Catalog => "catalog",
        cli_types::Capabilitykind::FileFormat => "file-format",
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

// The Direction-1 service sink: routes a loaded component's config/logging
// requests (expressed via ducklink-runtime's neutral types) to the wasm DuckDB
// core's config/logging guest interfaces.
/// v1.1 live-query host import: a host-side cache of recent catalog query results
/// (keyed by the SELECT text), refreshed at CLI statement boundaries when the
/// core is idle. It exists to solve the table-function RE-ENTRANCY wall: a
/// catalog component (autocomplete) calls `query` from INSIDE a running query, so
/// the live core executor is locked + the core wasm store is mid-call and cannot
/// be re-entered. The snapshot lets `query` still answer
/// `duckdb_tables()`/`duckdb_columns()` with the names captured just before the
/// completing query started (exactly what an editor autocomplete needs). Shared
/// between the CLI (`HostState`, which refreshes it after each `execute`) and
/// every component's `CoreServices` (which reads it when the core is busy).
#[derive(Default)]
struct CatalogSnapshot {
    rows: HashMap<String, Vec<Vec<String>>>,
    // Whether a query-capable extension is loaded; the CLI only pays for the
    // catalog refresh once one is (autocomplete sets this on load).
    enabled: bool,
}

struct CoreServices {
    core: Arc<Mutex<CoreExecution>>,
    // v1.1 live-query host import: the CLI's live connection, used by `query` to
    // run catalog SELECTs (e.g. autocomplete's table/column completion).
    current_connection: Arc<Mutex<Option<ResourceAny>>>,
    // v1.1 live-query host import: the re-entrancy fallback snapshot (see
    // CatalogSnapshot). Served when the core is busy (the table-function case).
    catalog_snapshot: Arc<Mutex<CatalogSnapshot>>,
}

impl CoreServices {
    fn with_core<F, R>(&self, f: F) -> R
    where
        F: FnOnce(&mut CoreExecution) -> R,
    {
        let mut core = self.core.lock().unwrap_or_else(|e| e.into_inner());
        f(&mut core)
    }
}

fn core_trap_to_config_error(err: wasmtime::Error) -> ConfigError {
    ConfigError::InternalConfig(err.to_string())
}

fn core_config_error_to_neutral(err: core_config_exports::Configerror) -> ConfigError {
    match err {
        core_config_exports::Configerror::Invalidkey(msg) => ConfigError::InvalidKey(msg.into()),
        core_config_exports::Configerror::Typemismatch(msg) => ConfigError::TypeMismatch(msg.into()),
        core_config_exports::Configerror::Unavailable(msg) => ConfigError::Unavailable(msg.into()),
        core_config_exports::Configerror::Internalconfig(msg) => {
            ConfigError::InternalConfig(msg.into())
        }
    }
}

fn neutral_loglevel_to_core(level: LogLevel) -> core_logging_exports::Loglevel {
    match level {
        LogLevel::Trace => core_logging_exports::Loglevel::Trace,
        LogLevel::Debug => core_logging_exports::Loglevel::Debug,
        LogLevel::Info => core_logging_exports::Loglevel::Info,
        LogLevel::Warn => core_logging_exports::Loglevel::Warn,
        LogLevel::Error => core_logging_exports::Loglevel::Error,
    }
}

impl ExtensionServices for CoreServices {
    fn provider_version(&mut self) -> Result<String, ConfigError> {
        self.with_core(|core| core.with_config(|guest, store| guest.call_provider_version(store)))
            .map_err(core_trap_to_config_error)
    }

    fn list_keys(&mut self, prefix: Option<&str>) -> Result<Vec<String>, ConfigError> {
        self.with_core(|core| core.with_config(|guest, store| guest.call_list_keys(store, prefix)))
            .map_err(core_trap_to_config_error)
    }

    fn get_string(&mut self, path: &str) -> Result<Option<String>, ConfigError> {
        self.with_core(|core| core.with_config(|guest, store| guest.call_get_string(store, path)))
            .map_err(core_trap_to_config_error)?
            .map_err(core_config_error_to_neutral)
    }

    fn get_bool(&mut self, path: &str) -> Result<Option<bool>, ConfigError> {
        self.with_core(|core| core.with_config(|guest, store| guest.call_get_bool(store, path)))
            .map_err(core_trap_to_config_error)?
            .map_err(core_config_error_to_neutral)
    }

    fn get_i64(&mut self, path: &str) -> Result<Option<i64>, ConfigError> {
        self.with_core(|core| core.with_config(|guest, store| guest.call_get_i64(store, path)))
            .map_err(core_trap_to_config_error)?
            .map_err(core_config_error_to_neutral)
    }

    fn get_u64(&mut self, path: &str) -> Result<Option<u64>, ConfigError> {
        self.with_core(|core| core.with_config(|guest, store| guest.call_get_u64(store, path)))
            .map_err(core_trap_to_config_error)?
            .map_err(core_config_error_to_neutral)
    }

    fn get_f64(&mut self, path: &str) -> Result<Option<f64>, ConfigError> {
        self.with_core(|core| core.with_config(|guest, store| guest.call_get_f64(store, path)))
            .map_err(core_trap_to_config_error)?
            .map_err(core_config_error_to_neutral)
    }

    fn get_bytes(&mut self, path: &str) -> Result<Option<Vec<u8>>, ConfigError> {
        self.with_core(|core| core.with_config(|guest, store| guest.call_get_bytes(store, path)))
            .map_err(core_trap_to_config_error)?
            .map_err(core_config_error_to_neutral)
    }

    fn get_string_list(&mut self, path: &str) -> Result<Option<Vec<String>>, ConfigError> {
        self.with_core(|core| {
            core.with_config(|guest, store| guest.call_get_string_list(store, path))
        })
        .map_err(core_trap_to_config_error)?
        .map_err(core_config_error_to_neutral)
    }

    fn log(&mut self, level: LogLevel, message: &str, target: Option<&str>) {
        let result = self.with_core(|core| {
            core.with_logging(|guest, store| {
                guest.call_log(store, neutral_loglevel_to_core(level), message, target)
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

    fn log_fields(&mut self, level: LogLevel, message: &str, fields: &[LogField]) {
        let converted: Vec<core_logging_exports::Logfield> = fields
            .iter()
            .map(|field| core_logging_exports::Logfield {
                key: field.key.clone().into(),
                value: field.value.clone().into(),
            })
            .collect();
        let result = self.with_core(|core| {
            core.with_logging(|guest, store| {
                guest.call_log_fields(
                    store,
                    neutral_loglevel_to_core(level),
                    message,
                    converted.as_slice(),
                )
            })
        });
        if let Err(err) = result {
            eprintln!("[duckdb-extension:{level:?}] {message} (core log_fields failed: {err})");
        }
    }

    // v1.1 live-query host import (catalog completion). Runs `sql` on the CLI's
    // live connection and returns rows of text cells (NULL -> "").
    //
    // RE-ENTRANCY GUARD: a table/scalar callback runs INSIDE the core query
    // engine, which means the single shared `core` mutex is ALREADY held by the
    // outer `call_execute` on the same thread AND the core wasm store is mid-call.
    // Re-entering would self-deadlock (the std Mutex is non-reentrant) and, even
    // past the lock, violate wasmtime store re-entrancy. So we `try_lock` the core
    // mutex: if it is contended we are nested in a query -> return Err and let the
    // caller (e.g. sql_complete) fall back to keyword-only completion. When the
    // core is idle (the import is reachable in non-table-fn contexts) the SELECT
    // runs and returns rows.
    fn query(&mut self, sql: &str) -> Result<Vec<Vec<String>>, String> {
        let core = match self.core.try_lock() {
            Ok(core) => core,
            Err(std::sync::TryLockError::WouldBlock) => {
                // BUSY (the table-function case): the live core executor is locked
                // by the query that called us, so we cannot run a live SELECT.
                // Fall back to the catalog snapshot captured at the last CLI
                // statement boundary. A miss returns Err -> keyword-only.
                return self
                    .catalog_snapshot
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .rows
                    .get(sql)
                    .cloned()
                    .ok_or_else(|| {
                        "query: core busy and no catalog snapshot for this SQL".to_string()
                    });
            }
            Err(std::sync::TryLockError::Poisoned(_)) => {
                return Err("query: core mutex poisoned".to_string())
            }
        };

        // IDLE: run live + refresh the snapshot entry so a later busy call hits.
        let rows = run_query_on_core(core, &self.current_connection, sql)?;
        self.catalog_snapshot
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .rows
            .insert(sql.to_string(), rows.clone());
        Ok(rows)
    }
}

/// Run `sql` on `current_connection` using the already-locked `core` executor,
/// returning rows of stringified cells (NULL -> ""). Factored out so both the
/// idle `query` path and the CLI-boundary refresh share one implementation.
/// True when `component` imports the `duckdb:extension/query` interface — i.e.
/// it can run live catalog SELECTs (autocomplete's catalog completion). Used to
/// gate the per-`execute` catalog-snapshot refresh so non-query extensions don't
/// pay for it. Best-effort: any import name in the `duckdb:extension` namespace
/// whose interface is `query` (with or without a `@version` suffix) counts.
fn component_imports_query(engine: &Engine, component: &Component) -> bool {
    component
        .component_type()
        .imports(engine)
        .any(|(name, _)| {
            // Instance import names look like `duckdb:extension/query` or
            // `duckdb:extension/query@1.1.0`.
            let iface = name.rsplit('/').next().unwrap_or(name);
            let iface = iface.split('@').next().unwrap_or(iface);
            name.starts_with("duckdb:extension/") && iface == "query"
        })
}

fn run_query_on_core(
    mut core: std::sync::MutexGuard<'_, CoreExecution>,
    current_connection: &Arc<Mutex<Option<ResourceAny>>>,
    sql: &str,
) -> Result<Vec<Vec<String>>, String> {
    let handle = current_connection
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .clone()
        .ok_or_else(|| "query: no active database connection".to_string())?;
    let result = core
        .with_database(|guest, store| guest.call_execute(store, handle, sql))
        .map_err(|trap| format!("query trapped: {trap}"))?;
    match result {
        Ok(qr) => Ok(qr
            .rows
            .iter()
            .map(|row| row.iter().map(spi_value_text).collect())
            .collect()),
        Err(err) => Err(core_duckerror_message(err)),
    }
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
    let component = load_component(engine, component_path).with_context(|| {
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
    core_storage_host::add_to_linker::<CoreStoreState, CoreStoreState>(
        &mut linker,
        |state| state,
    )?;
    core_index_host::add_to_linker::<CoreStoreState, CoreStoreState>(
        &mut linker,
        |state| state,
    )?;
    core_collation_host::add_to_linker::<CoreStoreState, CoreStoreState>(
        &mut linker,
        |state| state,
    )?;
    core_pragma_host::add_to_linker::<CoreStoreState, CoreStoreState>(
        &mut linker,
        |state| state,
    )?;
    core_files_host::add_to_linker::<CoreStoreState, CoreStoreState>(
        &mut linker,
        |state| state,
    )?;
    core_tvm_manager::add_to_linker::<CoreStoreState, CoreStoreState>(&mut linker, |state| state)?;
    core_tvm_bytes::add_to_linker::<CoreStoreState, CoreStoreState>(&mut linker, |state| state)?;

    let mut store = Store::new(
        engine,
        CoreStoreState {
            table: ResourceTable::new(),
            wasi: wasi_ctx,
            extension_manager,
            tvm: tvm_core::RegionDirectory::new(),
            tvm_slots: std::collections::HashMap::new(),
        },
    );

    let instance_pre = linker.instantiate_pre(&component)?;
    let pre = duckdb_core_bindings::LibduckdbPre::new(instance_pre)?;
    let bindings = pre.instantiate(store.as_context_mut())?;
    Ok(CoreExecution { store, bindings })
}

/// Load a component, deserializing a precompiled `.cwasm` (see
/// [`precompile_component_to_file`]) instead of Cranelift-compiling a `.wasm`.
/// A `.cwasm` makes even the first run fast (no compile); it is CPU- and
/// wasmtime-version-specific, and `deserialize` validates that before use.
fn load_component(engine: &Engine, path: &Path) -> Result<Component> {
    if path.extension().and_then(|s| s.to_str()) == Some("cwasm") {
        // SAFETY: trusts the file was produced by `precompile_component` against
        // a compatible engine; deserialize checks version/config and errors on
        // mismatch (it does not execute the contents).
        unsafe { Component::deserialize_file(engine, path) }
            .map_err(|e| e.context(format!("failed to deserialize precompiled {}", path.display())))
            .map_err(Into::into)
    } else {
        Component::from_file(engine, path)
            .map_err(|e| e.context(format!("failed to load {}", path.display())))
            .map_err(Into::into)
    }
}

/// AOT-compile a component `.wasm` to a `.cwasm` so the first run skips the
/// (~7s for the ~96 MB core) Cranelift compile. Output is CPU- and
/// wasmtime-version-specific; regenerate per target. Load it by passing the
/// `.cwasm` path wherever a component path is accepted.
pub fn precompile_component_to_file(in_path: &Path, out_path: &Path) -> Result<()> {
    let engine = build_engine()?;
    let bytes =
        std::fs::read(in_path).with_context(|| format!("read {}", in_path.display()))?;
    let precompiled = engine
        .precompile_component(&bytes)
        .map_err(|e| e.context(format!("precompile {}", in_path.display())))?;
    std::fs::write(out_path, &precompiled)
        .with_context(|| format!("write {}", out_path.display()))?;
    Ok(())
}

fn build_engine() -> Result<Engine> {
    let mut config = Config::new();
    config.wasm_component_model(true);
    // DuckDB (compiled with -fwasm-exceptions, standardized encoding) uses wasm
    // exception handling; enable the proposal so throws unwind and are caught
    // instead of aborting the module.
    config.wasm_exceptions(true);
    // Cache compiled artifacts on disk. The core component is ~96 MB of wasm;
    // Cranelift-compiling it from scratch costs ~7s and otherwise happens on
    // EVERY invocation (it dominates total runtime -- a trivial query takes as
    // long as a 20M-row sort). With the cache, the first run compiles + stores
    // and every later run deserializes in ~milliseconds (keyed by content +
    // compiler config + wasmtime version, so a rebuilt component recompiles once).
    match wasmtime::Cache::from_file(None) {
        Ok(cache) => {
            config.cache(Some(cache));
        }
        Err(err) => eprintln!("warning: wasmtime compile cache unavailable: {err}"),
    }
    Engine::new(&config).map_err(|e| e.context("failed to create Wasmtime engine").into())
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
    // Grant outbound network so wasi:sockets-backed code (e.g. httpfs over the
    // linked openssl/mbedtls + wasi-libc BSD sockets) can connect + resolve DNS.
    builder.inherit_network();
    builder.allow_ip_name_lookup(true);
    for (host, guest) in preopens {
        builder
            .preopened_dir(host, guest, DirPerms::all(), FilePerms::all())
            .map_err(|e| {
                e.context(format!(
                    "failed to preopen directory {} as {}",
                    host.display(),
                    guest
                ))
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
    builder.inherit_network();
    builder.allow_ip_name_lookup(true);
    for (host, guest) in preopens {
        builder
            .preopened_dir(host, guest, DirPerms::all(), FilePerms::all())
            .map_err(|e| {
                e.context(format!(
                    "failed to preopen directory {} as {}",
                    host.display(),
                    guest
                ))
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
            core_component: locate_component("ducklink_core.wasm")?,
            cli_component: locate_component("ducklink_cli.wasm")?,
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

/// v1.1 THE PIN — macro parameter names for a wrapper alias. Uses each arg's
/// declared name when present (and a valid identifier), else positional
/// `p0..pN`. A nameless or duplicate set is normalized to positional to keep
/// the generated `CREATE MACRO` well-formed.
fn macro_param_names(args: &[reg::FuncArg]) -> Vec<String> {
    let mut out: Vec<String> = Vec::with_capacity(args.len());
    let mut seen = std::collections::HashSet::new();
    for (i, a) in args.iter().enumerate() {
        let candidate = a
            .name
            .as_deref()
            .and_then(prefix::sanitize_prefix)
            .filter(|n| seen.insert(n.clone()));
        out.push(candidate.unwrap_or_else(|| format!("p{i}")));
    }
    out
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

fn convert_core_duckvalue_to_extension(value: core_types::Duckvalue) -> extension_types::Duckvalue {
    match value {
        core_types::Duckvalue::Null => extension_types::Duckvalue::Null,
        core_types::Duckvalue::Boolean(v) => extension_types::Duckvalue::Boolean(v),
        core_types::Duckvalue::Int64(v) => extension_types::Duckvalue::Int64(v),
        core_types::Duckvalue::Uint64(v) => extension_types::Duckvalue::Uint64(v),
        core_types::Duckvalue::Float64(v) => extension_types::Duckvalue::Float64(v),
        core_types::Duckvalue::Text(v) => extension_types::Duckvalue::Text(v),
        core_types::Duckvalue::Blob(v) => extension_types::Duckvalue::Blob(v),
        core_types::Duckvalue::Int32(v) => extension_types::Duckvalue::Int32(v),
        core_types::Duckvalue::Timestamp(v) => extension_types::Duckvalue::Timestamp(v),
        core_types::Duckvalue::Int8(v) => extension_types::Duckvalue::Int8(v),
        core_types::Duckvalue::Int16(v) => extension_types::Duckvalue::Int16(v),
        core_types::Duckvalue::Uint8(v) => extension_types::Duckvalue::Uint8(v),
        core_types::Duckvalue::Uint16(v) => extension_types::Duckvalue::Uint16(v),
        core_types::Duckvalue::Uint32(v) => extension_types::Duckvalue::Uint32(v),
        core_types::Duckvalue::Float32(v) => extension_types::Duckvalue::Float32(v),
        core_types::Duckvalue::Date(v) => extension_types::Duckvalue::Date(v),
        core_types::Duckvalue::Time(v) => extension_types::Duckvalue::Time(v),
        core_types::Duckvalue::Timestamptz(v) => extension_types::Duckvalue::Timestamptz(v),
        core_types::Duckvalue::Decimal(d) => {
            extension_types::Duckvalue::Decimal(extension_types::Decimalvalue {
                lower: d.lower,
                upper: d.upper,
                width: d.width,
                scale: d.scale,
            })
        }
        core_types::Duckvalue::Interval(iv) => {
            extension_types::Duckvalue::Interval(extension_types::Intervalvalue {
                months: iv.months,
                days: iv.days,
                micros: iv.micros,
            })
        }
        core_types::Duckvalue::Uuid(u) => {
            extension_types::Duckvalue::Uuid(extension_types::Uuidvalue { hi: u.hi, lo: u.lo })
        }
        core_types::Duckvalue::Complex(c) => {
            extension_types::Duckvalue::Complex(extension_types::Complexvalue {
                type_expr: c.type_expr,
                json: c.json,
            })
        }
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
        extension_types::Duckvalue::Int32(v) => core_types::Duckvalue::Int32(v),
        extension_types::Duckvalue::Timestamp(v) => core_types::Duckvalue::Timestamp(v),
        extension_types::Duckvalue::Int8(v) => core_types::Duckvalue::Int8(v),
        extension_types::Duckvalue::Int16(v) => core_types::Duckvalue::Int16(v),
        extension_types::Duckvalue::Uint8(v) => core_types::Duckvalue::Uint8(v),
        extension_types::Duckvalue::Uint16(v) => core_types::Duckvalue::Uint16(v),
        extension_types::Duckvalue::Uint32(v) => core_types::Duckvalue::Uint32(v),
        extension_types::Duckvalue::Float32(v) => core_types::Duckvalue::Float32(v),
        extension_types::Duckvalue::Date(v) => core_types::Duckvalue::Date(v),
        extension_types::Duckvalue::Time(v) => core_types::Duckvalue::Time(v),
        extension_types::Duckvalue::Timestamptz(v) => core_types::Duckvalue::Timestamptz(v),
        extension_types::Duckvalue::Decimal(d) => {
            core_types::Duckvalue::Decimal(core_types::Decimalvalue {
                lower: d.lower,
                upper: d.upper,
                width: d.width,
                scale: d.scale,
            })
        }
        extension_types::Duckvalue::Interval(iv) => {
            core_types::Duckvalue::Interval(core_types::Intervalvalue {
                months: iv.months,
                days: iv.days,
                micros: iv.micros,
            })
        }
        extension_types::Duckvalue::Uuid(u) => {
            core_types::Duckvalue::Uuid(core_types::Uuidvalue { hi: u.hi, lo: u.lo })
        }
        extension_types::Duckvalue::Complex(c) => {
            core_types::Duckvalue::Complex(core_types::Complexvalue {
                type_expr: c.type_expr,
                json: c.json,
            })
        }
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

// M2a: storage-host result converters (extension-WIT -> core-WIT).
fn convert_extension_logicaltype_to_core(
    ty: extension_types::Logicaltype,
) -> core_types::Logicaltype {
    match ty {
        extension_types::Logicaltype::Boolean => core_types::Logicaltype::Boolean,
        extension_types::Logicaltype::Int64 => core_types::Logicaltype::Int64,
        extension_types::Logicaltype::Uint64 => core_types::Logicaltype::Uint64,
        extension_types::Logicaltype::Float64 => core_types::Logicaltype::Float64,
        extension_types::Logicaltype::Text => core_types::Logicaltype::Text,
        extension_types::Logicaltype::Blob => core_types::Logicaltype::Blob,
        extension_types::Logicaltype::Int32 => core_types::Logicaltype::Int32,
        extension_types::Logicaltype::Timestamp => core_types::Logicaltype::Timestamp,
        extension_types::Logicaltype::Int8 => core_types::Logicaltype::Int8,
        extension_types::Logicaltype::Int16 => core_types::Logicaltype::Int16,
        extension_types::Logicaltype::Uint8 => core_types::Logicaltype::Uint8,
        extension_types::Logicaltype::Uint16 => core_types::Logicaltype::Uint16,
        extension_types::Logicaltype::Uint32 => core_types::Logicaltype::Uint32,
        extension_types::Logicaltype::Float32 => core_types::Logicaltype::Float32,
        extension_types::Logicaltype::Date => core_types::Logicaltype::Date,
        extension_types::Logicaltype::Time => core_types::Logicaltype::Time,
        extension_types::Logicaltype::Timestamptz => core_types::Logicaltype::Timestamptz,
        extension_types::Logicaltype::Decimal => core_types::Logicaltype::Decimal,
        extension_types::Logicaltype::Interval => core_types::Logicaltype::Interval,
        extension_types::Logicaltype::Uuid => core_types::Logicaltype::Uuid,
        extension_types::Logicaltype::Complex(expr) => core_types::Logicaltype::Complex(expr),
    }
}

fn convert_extension_columndef_to_core(col: extension_types::Columndef) -> core_types::Columndef {
    core_types::Columndef {
        name: col.name,
        logical: convert_extension_logicaltype_to_core(col.logical),
    }
}

// M2b: convert a core-WIT scan-request into the storage-interface scan-request
// the backing component's storage-dispatch expects.
fn convert_core_compare_op_to_storage(op: core_storage_host::CompareOp) -> storage_scan::CompareOp {
    match op {
        core_storage_host::CompareOp::Eq => storage_scan::CompareOp::Eq,
        core_storage_host::CompareOp::Ne => storage_scan::CompareOp::Ne,
        core_storage_host::CompareOp::Lt => storage_scan::CompareOp::Lt,
        core_storage_host::CompareOp::Le => storage_scan::CompareOp::Le,
        core_storage_host::CompareOp::Gt => storage_scan::CompareOp::Gt,
        core_storage_host::CompareOp::Ge => storage_scan::CompareOp::Ge,
        core_storage_host::CompareOp::IsNull => storage_scan::CompareOp::IsNull,
        core_storage_host::CompareOp::IsNotNull => storage_scan::CompareOp::IsNotNull,
    }
}

fn convert_core_duckvalue_to_storage(value: core_types::Duckvalue) -> storage_scan::Duckvalue {
    match value {
        core_types::Duckvalue::Null => storage_scan::Duckvalue::Null,
        core_types::Duckvalue::Boolean(v) => storage_scan::Duckvalue::Boolean(v),
        core_types::Duckvalue::Int64(v) => storage_scan::Duckvalue::Int64(v),
        core_types::Duckvalue::Uint64(v) => storage_scan::Duckvalue::Uint64(v),
        core_types::Duckvalue::Float64(v) => storage_scan::Duckvalue::Float64(v),
        core_types::Duckvalue::Text(v) => storage_scan::Duckvalue::Text(v),
        core_types::Duckvalue::Blob(v) => storage_scan::Duckvalue::Blob(v),
        core_types::Duckvalue::Int32(v) => storage_scan::Duckvalue::Int32(v),
        core_types::Duckvalue::Timestamp(v) => storage_scan::Duckvalue::Timestamp(v),
        core_types::Duckvalue::Int8(v) => storage_scan::Duckvalue::Int8(v),
        core_types::Duckvalue::Int16(v) => storage_scan::Duckvalue::Int16(v),
        core_types::Duckvalue::Uint8(v) => storage_scan::Duckvalue::Uint8(v),
        core_types::Duckvalue::Uint16(v) => storage_scan::Duckvalue::Uint16(v),
        core_types::Duckvalue::Uint32(v) => storage_scan::Duckvalue::Uint32(v),
        core_types::Duckvalue::Float32(v) => storage_scan::Duckvalue::Float32(v),
        core_types::Duckvalue::Date(v) => storage_scan::Duckvalue::Date(v),
        core_types::Duckvalue::Time(v) => storage_scan::Duckvalue::Time(v),
        core_types::Duckvalue::Timestamptz(v) => storage_scan::Duckvalue::Timestamptz(v),
        core_types::Duckvalue::Decimal(d) => {
            storage_scan::Duckvalue::Decimal(storage_scan::Decimalvalue {
                lower: d.lower,
                upper: d.upper,
                width: d.width,
                scale: d.scale,
            })
        }
        core_types::Duckvalue::Interval(iv) => {
            storage_scan::Duckvalue::Interval(storage_scan::Intervalvalue {
                months: iv.months,
                days: iv.days,
                micros: iv.micros,
            })
        }
        core_types::Duckvalue::Uuid(u) => {
            storage_scan::Duckvalue::Uuid(storage_scan::Uuidvalue { hi: u.hi, lo: u.lo })
        }
        core_types::Duckvalue::Complex(c) => {
            storage_scan::Duckvalue::Complex(storage_scan::Complexvalue {
                type_expr: c.type_expr,
                json: c.json,
            })
        }
    }
}

fn convert_core_scan_request_to_storage(
    request: core_storage_host::ScanRequest,
) -> storage_scan::ScanRequest {
    storage_scan::ScanRequest {
        table: request.table,
        projection: request.projection,
        filters: request
            .filters
            .into_iter()
            .map(|f| storage_scan::ScanFilter {
                column: f.column,
                op: convert_core_compare_op_to_storage(f.op),
                value: convert_core_duckvalue_to_storage(f.value),
            })
            .collect(),
        limit: request.limit,
    }
}

/// Short human-readable rendering of a core Duckvalue for the pushdown log line.
fn describe_core_duckvalue(value: &core_types::Duckvalue) -> String {
    match value {
        core_types::Duckvalue::Null => "NULL".to_string(),
        core_types::Duckvalue::Boolean(v) => v.to_string(),
        core_types::Duckvalue::Int64(v) => v.to_string(),
        core_types::Duckvalue::Uint64(v) => v.to_string(),
        core_types::Duckvalue::Float64(v) => v.to_string(),
        core_types::Duckvalue::Text(v) => format!("{v:?}"),
        core_types::Duckvalue::Blob(v) => format!("<blob {} bytes>", v.len()),
        core_types::Duckvalue::Int32(v) => v.to_string(),
        core_types::Duckvalue::Timestamp(v) => v.to_string(),
        core_types::Duckvalue::Int8(v) => v.to_string(),
        core_types::Duckvalue::Int16(v) => v.to_string(),
        core_types::Duckvalue::Uint8(v) => v.to_string(),
        core_types::Duckvalue::Uint16(v) => v.to_string(),
        core_types::Duckvalue::Uint32(v) => v.to_string(),
        core_types::Duckvalue::Float32(v) => v.to_string(),
        core_types::Duckvalue::Date(v) => v.to_string(),
        core_types::Duckvalue::Time(v) => v.to_string(),
        core_types::Duckvalue::Timestamptz(v) => v.to_string(),
        core_types::Duckvalue::Decimal(d) => format_decimal(d.lower, d.upper, d.width, d.scale),
        core_types::Duckvalue::Interval(iv) => {
            format!("{}mon {}d {}us", iv.months, iv.days, iv.micros)
        }
        core_types::Duckvalue::Uuid(u) => format_uuid(u.hi, u.lo),
        core_types::Duckvalue::Complex(c) => format!("{}:{}", c.type_expr, c.json),
    }
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
        let current_connection = Arc::new(Mutex::new(None));
        let catalog_snapshot;
        {
            let mut manager = extension_manager
                .lock()
                .expect("extension manager mutex poisoned");
            manager.attach_current_connection(current_connection.clone());
            catalog_snapshot = manager.catalog_snapshot();
        }
        let dotcmd_registry = Arc::new(Mutex::new(DotcmdRegistry::load(
            &engine,
            &dotcmd_root(),
            core.clone(),
            current_connection.clone(),
            extension_manager.clone(),
        )));
        let host_state = HostState {
            table: ResourceTable::new(),
            wasi: cli_wasi,
            core: core.clone(),
            extension_manager: extension_manager.clone(),
            dotcmd_registry,
            current_connection,
            next_resource_id: 1,
            connections: HashMap::new(),
            streams: HashMap::new(),
            prepared: HashMap::new(),
            appenders: HashMap::new(),
            pending_connection_drops: Vec::new(),
            pending_stream_drops: Vec::new(),
            pending_prepared_drops: Vec::new(),
            pending_appender_drops: Vec::new(),
            did_autoload: false,
            catalog_snapshot,
            preopens: owned_preopens.clone(),
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

        // The CLI routes an unknown `.NAME args` here; the host invokes the
        // owning pluggable dot-command component and returns its output.
        let mut dotcmd_host = linker.instance("duckdb:cli/dotcmd-host")?;
        dotcmd_host.func_wrap(
            "invoke",
            |store: StoreContextMut<'_, HostState>, (name, args): (String, String)| {
                let registry = store.data().dotcmd_registry.clone();
                let mut registry = registry.lock().unwrap_or_else(|e| e.into_inner());
                let result = match registry.invoke(&name, &args) {
                    None => Ok(None),
                    Some(Ok((text, deltas))) => Ok(Some(make_cli_outcome(text, deltas))),
                    Some(Err(message)) => Err(message),
                };
                Ok((result,))
            },
        )?;
        dotcmd_host.func_wrap(
            "list-commands",
            |store: StoreContextMut<'_, HostState>, (): ()| Ok((cli_command_infos(&store),)),
        )?;

        let cli_component =
            load_component(&engine, &artifacts.cli_component).with_context(|| {
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
            .map_err(|e| e.context(format!("failed to preload extension {name}")))?;
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
    let owned_preopens = resolve_preopens_with_default(preopens)?;
    let preopen_refs: Vec<(&Path, &str)> = owned_preopens
        .iter()
        .map(|(host, guest)| (host.as_path(), guest.as_str()))
        .collect();
    let args_vec: Vec<String> = args.iter().map(|s| s.as_ref().to_owned()).collect();
    let cli_wasi = build_wasi_ctx_inherit(&args_vec, &preopen_refs)?;
    let core_wasi = build_wasi_ctx_inherit(&[String::from("duckdb-core")], &preopen_refs)?;

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
    let current_connection = Arc::new(Mutex::new(None));
    let catalog_snapshot;
    {
        let mut manager = extension_manager
            .lock()
            .expect("extension manager mutex poisoned");
        manager.attach_current_connection(current_connection.clone());
        catalog_snapshot = manager.catalog_snapshot();
    }
    let dotcmd_registry = Arc::new(Mutex::new(DotcmdRegistry::load(
        &engine,
        &dotcmd_root(),
        core.clone(),
        current_connection.clone(),
        extension_manager.clone(),
    )));
    let host_state = HostState {
        table: ResourceTable::new(),
        wasi: cli_wasi,
        core: core.clone(),
        extension_manager: extension_manager.clone(),
        dotcmd_registry,
        current_connection,
        next_resource_id: 1,
        connections: HashMap::new(),
        streams: HashMap::new(),
        prepared: HashMap::new(),
        appenders: HashMap::new(),
        pending_connection_drops: Vec::new(),
        pending_stream_drops: Vec::new(),
        pending_prepared_drops: Vec::new(),
        pending_appender_drops: Vec::new(),
        did_autoload: false,
        catalog_snapshot,
        preopens: owned_preopens.clone(),
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
    let mut dotcmd_host = linker.instance("duckdb:cli/dotcmd-host")?;
    dotcmd_host.func_wrap(
        "invoke",
        |store: StoreContextMut<'_, HostState>, (name, args): (String, String)| {
            let registry = store.data().dotcmd_registry.clone();
            let mut registry = registry.lock().unwrap_or_else(|e| e.into_inner());
            let result = match registry.invoke(&name, &args) {
                None => Ok(None),
                Some(Ok((text, deltas))) => Ok(Some(make_cli_outcome(text, deltas))),
                Some(Err(message)) => Err(message),
            };
            Ok((result,))
        },
    )?;
    dotcmd_host.func_wrap(
        "list-commands",
        |store: StoreContextMut<'_, HostState>, (): ()| Ok((cli_command_infos(&store),)),
    )?;

    let cli_component =
        load_component(&engine, &artifacts.cli_component).with_context(|| {
            format!(
                "failed to load CLI component from {}",
                artifacts.cli_component.display()
            )
        })?;
    let instance_pre = linker.instantiate_pre(&cli_component)?;
    let cli_pre = duckdb_cli_bindings::DuckdbCliPre::new(instance_pre)?;
    let cli = cli_pre.instantiate(store.as_context_mut())?;

    Ok(cli.wasi_cli_run().call_run(store.as_context_mut())?)
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

    #[test]
    fn core_appender_bulk_inserts_under_wasmtime() -> Result<()> {
        let engine = build_engine()?;
        let artifacts = ComponentArtifacts::resolve_default()?;
        let wasi = build_wasi_ctx_inherit(&[String::from("duckdb-core")], &[])?;
        let manager = Arc::new(Mutex::new(ExtensionManager::new(engine.clone())));
        let mut core = instantiate_core(&engine, &artifacts.core_component, wasi, manager)?;

        let conn = core
            .with_database(|g, s| g.call_open(s, None))?
            .map_err(|e| anyhow::anyhow!("open: {e}"))?;
        core.with_database(|g, s| {
            g.call_execute(s, conn.clone(), "CREATE TABLE t(id BIGINT, name VARCHAR)")
        })?
        .map_err(|e| anyhow::anyhow!("create: {e:?}"))?;

        // Bulk-insert rows through the appender.
        let appender = core
            .with_database(|g, s| g.call_create_appender(s, conn.clone(), None, "t"))?
            .map_err(|e| anyhow::anyhow!("create_appender: {e:?}"))?;
        for (id, name) in [(1i64, "alice"), (2, "bob"), (3, "carol")] {
            let values = vec![
                core_types::Duckvalue::Int64(id),
                core_types::Duckvalue::Text(name.to_string()),
            ];
            core.with_appender(|g, s| g.call_append_row(s, appender.clone(), &values))?
                .map_err(|e| anyhow::anyhow!("append_row: {e:?}"))?;
        }
        core.with_appender(|g, s| g.call_flush(s, appender.clone()))?
            .map_err(|e| anyhow::anyhow!("flush: {e:?}"))?;

        // Read the appended rows back.
        let result = core
            .with_database(|g, s| {
                g.call_execute(s, conn, "SELECT count(*) AS n, sum(id) AS total FROM t")
            })?
            .map_err(|e| anyhow::anyhow!("select: {e:?}"))?;
        let cell = |row: usize, col: usize| -> String {
            match result.rows.get(row).and_then(|r| r.get(col)) {
                Some(core_types::Duckvalue::Int64(v)) => v.to_string(),
                Some(core_types::Duckvalue::Uint64(v)) => v.to_string(),
                Some(core_types::Duckvalue::Text(v)) => v.clone(),
                other => format!("{other:?}"),
            }
        };
        assert_eq!(cell(0, 0), "3", "appended row count");
        assert_eq!(cell(0, 1), "6", "sum of appended ids");

        Ok(())
    }

    #[test]
    fn core_prepared_statement_binds_and_reuses_under_wasmtime() -> Result<()> {
        // Drives the core's prepared-statement API directly through wasmtime
        // (the runtime the standalone and host use), complementing the browser
        // (jco) verification of the same core component.
        let engine = build_engine()?;
        let artifacts = ComponentArtifacts::resolve_default()?;
        let wasi = build_wasi_ctx_inherit(&[String::from("duckdb-core")], &[])?;
        let manager = Arc::new(Mutex::new(ExtensionManager::new(engine.clone())));
        let mut core = instantiate_core(&engine, &artifacts.core_component, wasi, manager)?;

        let conn = core
            .with_database(|guest, store| guest.call_open(store, None))?
            .map_err(|e| anyhow::anyhow!("open failed: {e}"))?;

        let stmt = core
            .with_database(|guest, store| {
                guest.call_prepare(
                    store,
                    conn.clone(),
                    "SELECT CAST($1 AS BIGINT) + CAST($2 AS BIGINT) AS total",
                )
            })?
            .map_err(|e| anyhow::anyhow!("prepare failed: {e:?}"))?;

        let count =
            core.with_prepared(|guest, store| guest.call_parameter_count(store, stmt.clone()))?;
        assert_eq!(count, 2, "expected two parameters");

        let run = |core: &mut CoreExecution, a: i64, b: i64| -> Result<String> {
            let params = vec![
                core_types::Duckvalue::Int64(a),
                core_types::Duckvalue::Int64(b),
            ];
            let result = core
                .with_prepared(|guest, store| guest.call_execute(store, stmt.clone(), &params))?
                .map_err(|e| anyhow::anyhow!("execute failed: {e:?}"))?;
            let cell = result
                .rows
                .first()
                .and_then(|row| row.first())
                .cloned()
                .ok_or_else(|| anyhow::anyhow!("no result cell"))?;
            Ok(match cell {
                core_types::Duckvalue::Text(v) => v,
                core_types::Duckvalue::Int64(v) => v.to_string(),
                other => format!("{other:?}"),
            })
        };

        assert_eq!(run(&mut core, 40, 2)?, "42");
        assert_eq!(run(&mut core, 100, 1)?, "101", "prepared statement reuse");

        Ok(())
    }

    #[test]
    fn core_query_arrow_produces_valid_ipc_stream() -> Result<()> {
        use arrow_array::cast::AsArray;
        use arrow_array::types::Int32Type;

        let engine = build_engine()?;
        let artifacts = ComponentArtifacts::resolve_default()?;
        let wasi = build_wasi_ctx_inherit(&[String::from("duckdb-core")], &[])?;
        let manager = Arc::new(Mutex::new(ExtensionManager::new(engine.clone())));
        let mut core = instantiate_core(&engine, &artifacts.core_component, wasi, manager)?;

        let conn = core
            .with_database(|guest, store| guest.call_open(store, None))?
            .map_err(|e| anyhow::anyhow!("open failed: {e}"))?;

        let bytes = core
            .with_database(|guest, store| {
                guest.call_query_arrow(
                    store,
                    conn,
                    "SELECT i::INTEGER AS n FROM range(5) t(i)",
                )
            })?
            .map_err(|e| anyhow::anyhow!("query_arrow failed: {e:?}"))?;

        // Decode the IPC stream with an independent Arrow implementation.
        let reader = arrow_ipc::reader::StreamReader::try_new(std::io::Cursor::new(bytes), None)
            .context("arrow IPC stream did not decode")?;
        let mut values = Vec::new();
        for batch in reader {
            let batch = batch?;
            assert_eq!(batch.schema().field(0).name(), "n");
            let col = batch.column(0).as_primitive::<Int32Type>();
            for i in 0..batch.num_rows() {
                values.push(col.value(i));
            }
        }
        assert_eq!(values, vec![0, 1, 2, 3, 4], "round-tripped arrow column");

        Ok(())
    }

    #[test]
    fn core_open_with_config_can_disable_external_access() -> Result<()> {
        // The filesystem sandbox is the WASI preopen shims; this verifies the
        // one DuckDB-level hardening knob that works in wasm — disabling external
        // file access (read_csv/read_text/COPY) as an opt-in via open-with-config.
        let engine = build_engine()?;
        let artifacts = ComponentArtifacts::resolve_default()?;
        let tempdir = tempdir().context("failed to create temporary directory")?;
        std::fs::write(tempdir.path().join("d.csv"), "a,b\n1,x\n2,y\n")?;
        let preopens = [(tempdir.path(), ".")];
        let manager = Arc::new(Mutex::new(ExtensionManager::new(engine.clone())));
        let read = "SELECT count(*) AS n FROM read_csv_auto('d.csv')";

        // Default: external access enabled, read_csv works.
        let wasi = build_wasi_ctx_inherit(&[String::from("duckdb-core")], &preopens)?;
        let mut core = instantiate_core(&engine, &artifacts.core_component, wasi, manager.clone())?;
        let conn = core
            .with_database(|g, s| g.call_open(s, None))?
            .map_err(|e| anyhow::anyhow!("open: {e}"))?;
        let allowed = core.with_database(|g, s| g.call_execute(s, conn, read))?;
        assert!(allowed.is_ok(), "read_csv should work by default: {allowed:?}");

        // Opt-in hardening: enable_external_access=false blocks read_csv.
        let wasi = build_wasi_ctx_inherit(&[String::from("duckdb-core")], &preopens)?;
        let mut core = instantiate_core(&engine, &artifacts.core_component, wasi, manager)?;
        let opts = vec![("enable_external_access".to_string(), "false".to_string())];
        let conn = core
            .with_database(|g, s| g.call_open_with_config(s, None, &opts))?
            .map_err(|e| anyhow::anyhow!("open_with_config: {e}"))?;
        let blocked = core.with_database(|g, s| g.call_execute(s, conn, read))?;
        assert!(
            blocked.is_err(),
            "read_csv should be blocked when external access is disabled, got {blocked:?}"
        );

        Ok(())
    }

    #[test]
    fn core_open_with_config_applies_and_rejects_options() -> Result<()> {
        let engine = build_engine()?;
        let artifacts = ComponentArtifacts::resolve_default()?;
        let manager = Arc::new(Mutex::new(ExtensionManager::new(engine.clone())));

        // A valid option is applied to the connection.
        let wasi = build_wasi_ctx_inherit(&[String::from("duckdb-core")], &[])?;
        let mut core = instantiate_core(&engine, &artifacts.core_component, wasi, manager.clone())?;
        // default_order defaults to ASC; setting it at open time should stick.
        let options = vec![("default_order".to_string(), "desc".to_string())];
        let conn = core
            .with_database(|guest, store| guest.call_open_with_config(store, None, &options))?
            .map_err(|e| anyhow::anyhow!("open_with_config failed: {e}"))?;
        let result = core
            .with_database(|guest, store| {
                guest.call_execute(
                    store,
                    conn,
                    "SELECT current_setting('default_order') AS v",
                )
            })?
            .map_err(|e| anyhow::anyhow!("execute failed: {e:?}"))?;
        let cell = result
            .rows
            .first()
            .and_then(|row| row.first())
            .cloned()
            .ok_or_else(|| anyhow::anyhow!("no result cell"))?;
        let rendered = match cell {
            core_types::Duckvalue::Text(v) => v,
            core_types::Duckvalue::Int64(v) => v.to_string(),
            other => format!("{other:?}"),
        };
        assert_eq!(rendered, "DESC", "default_order config option should be applied");

        // An invalid value for a known option fails the open.
        let wasi = build_wasi_ctx_inherit(&[String::from("duckdb-core")], &[])?;
        let mut core = instantiate_core(&engine, &artifacts.core_component, wasi, manager)?;
        let bad = vec![("access_mode".to_string(), "definitely_not_a_mode".to_string())];
        let outcome =
            core.with_database(|guest, store| guest.call_open_with_config(store, None, &bad))?;
        assert!(
            outcome.is_err(),
            "expected an invalid config value to fail the open, got {outcome:?}"
        );

        Ok(())
    }

    #[test]
    #[ignore = "embedded sqlite_scanner: the lean default core embeds no officials \
                (ships sqlite as the sqlitewasm component); run on a fat core with \
                `cargo test -- --ignored` or EMBED_EXTENSIONS=sqlite_scanner"]
    fn sqlite_scanner_embedded_attach_and_query() -> Result<()> {
        // Exercise the embedded sqlite_scanner end to end: ATTACH an in-memory
        // SQLite database (sqlite_scanner's sqlite3 calls resolve to the shared
        // amalgamation after the collision fix), write a row, read it back --
        // proving sqlite_scanner is functional in the full-embed core, not just
        // loaded. Skips silently if sqlite_scanner is not embedded in the core.
        let tempdir = tempdir().context("failed to create temporary directory")?;
        let preopens = [(tempdir.path(), ".")];
        let sql = "ATTACH ':memory:' AS s (TYPE sqlite); \
                   CREATE TABLE s.t(i INTEGER); \
                   INSERT INTO s.t VALUES (42); \
                   SELECT i AS sqlite_val FROM s.t;";
        let args = ["duckdb-cli", "-c", sql];
        let mut h = CliHarness::new(&args, &preopens)?;
        let status = h.run()?;
        let stdout = h.stdout().unwrap_or_default();
        let stderr = h.stderr().unwrap_or_default();
        if stderr.to_lowercase().contains("sqlite")
            && stderr.to_lowercase().contains("not found")
        {
            eprintln!("sqlite_scanner not embedded in this core; skipping");
            return Ok(());
        }
        if status.is_err() {
            panic!("sqlite_scanner CLI error\nstdout:\n{stdout}\nstderr:\n{stderr}");
        }
        assert!(
            has_cell(&stdout, "42"),
            "expected sqlite_scanner ATTACH+query to return 42, got:\n{stdout}\nstderr:\n{stderr}"
        );
        Ok(())
    }

    #[test]
    #[ignore = "embedded delta extension: the lean default core embeds no officials \
                (ships delta metadata as the deltascan component); run on a fat core \
                with `cargo test -- --ignored` or EMBED_EXTENSIONS=delta"]
    fn delta_scan_embedded_local_table() -> Result<()> {
        // Exercise the embedded delta extension (duckdb-delta @ 45c40878 +
        // delta-kernel-rs v0.21.0 sync engine) end to end: copy a local Delta
        // table (the canonical `simple_table` fixture: one BIGINT column `i` with
        // 10 rows, snappy parquet) into a preopened dir and read it back via
        // delta_scan(). Proves the sync-engine kernel + the full extension work in
        // the core, not just link. Skips if delta is not embedded or the fixture
        // (shipped in the vendored duckdb-delta checkout) is absent.
        let fixture = workspace_root()
            .parent()
            .map(|p| {
                p.join("duckdb-wasm/build/duckdb-delta/data/inlined/simple_table/delta_lake")
            })
            .filter(|p| p.join("_delta_log").is_dir());
        let Some(fixture) = fixture else {
            eprintln!("delta simple_table fixture not found; skipping");
            return Ok(());
        };
        let tempdir = tempdir().context("failed to create temporary directory")?;
        // Recursively copy the table into the preopened dir (guest path ".").
        fn copy_dir(src: &std::path::Path, dst: &std::path::Path) -> std::io::Result<()> {
            std::fs::create_dir_all(dst)?;
            for entry in std::fs::read_dir(src)? {
                let entry = entry?;
                let to = dst.join(entry.file_name());
                if entry.file_type()?.is_dir() {
                    copy_dir(&entry.path(), &to)?;
                } else {
                    std::fs::copy(entry.path(), &to)?;
                }
            }
            Ok(())
        }
        copy_dir(&fixture, &tempdir.path().join("simple_table"))
            .context("failed to copy delta fixture")?;
        let preopens = [(tempdir.path(), ".")];
        let sql = "SELECT count(*) AS n, sum(i) AS s FROM delta_scan('simple_table');";
        let args = ["duckdb-cli", "-c", sql];
        let mut h = CliHarness::new(&args, &preopens)?;
        let status = h.run()?;
        let stdout = h.stdout().unwrap_or_default();
        let stderr = h.stderr().unwrap_or_default();
        let low = stderr.to_lowercase();
        if (low.contains("delta") || low.contains("delta_scan"))
            && (low.contains("not found") || low.contains("does not exist"))
        {
            eprintln!("delta not embedded in this core; skipping");
            return Ok(());
        }
        if status.is_err() {
            panic!("delta_scan CLI error\nstdout:\n{stdout}\nstderr:\n{stderr}");
        }
        assert!(
            has_cell(&stdout, "10"),
            "expected delta_scan('simple_table') to return 10 rows, got:\n{stdout}\nstderr:\n{stderr}"
        );
        Ok(())
    }

    #[test]
    fn unity_catalog_embedded_loaded_and_type_registered() -> Result<()> {
        // Exercise the embedded unity_catalog extension (duckdb 1.5.4 renamed
        // uc_catalog -> unity_catalog @ d52a7ee; REST over DuckDB's HTTPUtil/curl).
        // It ATTACHes a remote Unity Catalog (needs a live server + token), so we
        // can't reach a real catalog here; instead prove it loads in the core and
        // that the `unity_catalog` storage type is registered (ATTACH with a bogus
        // endpoint fails connecting/authorizing, NOT with "unknown catalog type").
        // Skips if unity_catalog is not embedded.
        let preopens: [(&std::path::Path, &str); 0] = [];
        let loaded = {
            let sql = "SELECT count(*) AS n FROM duckdb_extensions() \
                       WHERE extension_name = 'unity_catalog' AND loaded;";
            let args = ["duckdb-cli", "-c", sql];
            let mut h = CliHarness::new(&args, &preopens)?;
            if h.run()?.is_err() {
                eprintln!("unity_catalog extensions query failed; skipping");
                return Ok(());
            }
            has_cell(&h.stdout().unwrap_or_default(), "1")
        };
        if !loaded {
            eprintln!("unity_catalog not embedded/loaded in this core; skipping");
            return Ok(());
        }
        // ATTACH with TYPE unity_catalog: a registered type fails connecting to the
        // bogus endpoint, NOT with "unknown/unsupported catalog type".
        let sql = "ATTACH 'bogus' AS uc (TYPE unity_catalog, \
                   ENDPOINT 'http://127.0.0.1:1', TOKEN 'x'); SELECT 1;";
        let args = ["duckdb-cli", "-c", sql];
        let mut h = CliHarness::new(&args, &preopens)?;
        let _ = h.run()?;
        let stderr = h.stderr().unwrap_or_default().to_lowercase();
        assert!(
            !stderr.contains("unknown catalog type") && !stderr.contains("unsupported")
                && !stderr.contains("not found for type"),
            "TYPE unity_catalog not registered by the extension; got:\n{stderr}"
        );
        Ok(())
    }

    #[test]
    fn iceberg_scan_embedded_local_table() -> Result<()> {
        // Exercise the embedded iceberg extension (duckdb-iceberg @ e6fe0a4b, built
        // against minimal AWS-type stubs since the AWS C++ SDK doesn't build for
        // wasm) end to end: read a local Iceberg table (the `partition_bool` fixture
        // -- 2 records, avro manifests + snappy parquet) via iceberg_scan(). Proves
        // the avro-manifest + roaring + parquet read path works in the core. Skips
        // if iceberg is not embedded or the fixture (in the vendored duckdb-iceberg
        // checkout) is absent.
        let fixture = workspace_root().parent().map(|p| {
            p.join(
                "duckdb-wasm/build/duckdb-wasi/_deps/iceberg_extension_fc-src/\
                 data/persistent/partition_bool",
            )
        });
        let Some(fixture) = fixture.filter(|p| p.join("metadata/version-hint.text").is_file())
        else {
            eprintln!("iceberg partition_bool fixture not found; skipping");
            return Ok(());
        };
        let tempdir = tempdir().context("failed to create temporary directory")?;
        fn copy_dir(src: &std::path::Path, dst: &std::path::Path) -> std::io::Result<()> {
            std::fs::create_dir_all(dst)?;
            for entry in std::fs::read_dir(src)? {
                let entry = entry?;
                let to = dst.join(entry.file_name());
                if entry.file_type()?.is_dir() {
                    copy_dir(&entry.path(), &to)?;
                } else {
                    std::fs::copy(entry.path(), &to)?;
                }
            }
            Ok(())
        }
        // The fixture's metadata embeds the table's ORIGINAL relative path
        // (data/persistent/partition_bool/metadata/...), so replicate that subpath
        // in the preopened dir and scan it there -- otherwise the manifest-list
        // avro resolves to a path that does not exist.
        let rel = std::path::Path::new("data/persistent/partition_bool");
        copy_dir(&fixture, &tempdir.path().join(rel)).context("failed to copy iceberg fixture")?;
        let preopens = [(tempdir.path(), ".")];
        let sql = "SELECT count(*) AS n FROM iceberg_scan('data/persistent/partition_bool');";
        let args = ["duckdb-cli", "-c", sql];
        let mut h = CliHarness::new(&args, &preopens)?;
        let status = h.run()?;
        let stdout = h.stdout().unwrap_or_default();
        let stderr = h.stderr().unwrap_or_default();
        let low = stderr.to_lowercase();
        if (low.contains("iceberg") || low.contains("iceberg_scan"))
            && (low.contains("not found") || low.contains("does not exist")
                || low.contains("catalog error"))
        {
            eprintln!("iceberg not embedded in this core; skipping");
            return Ok(());
        }
        if status.is_err() {
            panic!("iceberg_scan CLI error\nstdout:\n{stdout}\nstderr:\n{stderr}");
        }
        assert!(
            has_cell(&stdout, "2"),
            "expected iceberg_scan('ice_table') to return 2 rows, got:\n{stdout}\nstderr:\n{stderr}"
        );
        Ok(())
    }

    #[test]
    fn azure_embedded_loaded_and_scheme_registered() -> Result<()> {
        // Exercise the embedded azure extension (duckdb-azure @ 563589b2 + the Azure
        // SDK for C++ built for wasm). Azure is a remote filesystem (az://) needing a
        // live account + network, so we can't read real blobs here; instead prove it
        // is loaded in the core and that the az:// scheme is registered (a secretless
        // read fails with an azure/secret error, NOT "unknown file system"). Skips if
        // azure is not embedded.
        let preopens: [(&std::path::Path, &str); 0] = [];
        let loaded = {
            let sql = "SELECT count(*) AS n FROM duckdb_extensions() \
                       WHERE extension_name = 'azure' AND loaded;";
            let args = ["duckdb-cli", "-c", sql];
            let mut h = CliHarness::new(&args, &preopens)?;
            let status = h.run()?;
            let stdout = h.stdout().unwrap_or_default();
            if status.is_err() {
                eprintln!("azure extensions query failed; skipping\n{}", h.stderr().unwrap_or_default());
                return Ok(());
            }
            has_cell(&stdout, "1")
        };
        if !loaded {
            eprintln!("azure not embedded/loaded in this core; skipping");
            return Ok(());
        }
        // az:// scheme registered: a secretless read errors azure-side, not "unknown
        // file system" (which is what an unregistered scheme would report).
        let sql = "SELECT * FROM read_parquet('az://acct/cont/none.parquet');";
        let args = ["duckdb-cli", "-c", sql];
        let mut h = CliHarness::new(&args, &preopens)?;
        let _ = h.run()?;
        let stderr = h.stderr().unwrap_or_default().to_lowercase();
        assert!(
            !stderr.contains("unknown file system") && !stderr.contains("unknown filesystem"),
            "az:// scheme not registered by azure extension; got:\n{stderr}"
        );
        Ok(())
    }

    #[test]
    fn ui_embedded_start_ui_initializes_bridge() -> Result<()> {
        // Exercise the embedded ui extension (duckdb-ui @ a135471). The native host
        // owns the listening socket and bridges requests to duckdb_ui_handle_request;
        // here we just prove the extension loads in the core and that `start_ui()`
        // initializes the bridged HttpServer singleton (returns "UI started at ...",
        // not an error) -- on wasm Start() runs without a listening thread/system().
        // Skips if ui is not embedded.
        let preopens: [(&std::path::Path, &str); 0] = [];
        let loaded = {
            let sql = "SELECT count(*) AS n FROM duckdb_extensions() \
                       WHERE extension_name = 'ui' AND loaded;";
            let args = ["duckdb-cli", "-c", sql];
            let mut h = CliHarness::new(&args, &preopens)?;
            if h.run()?.is_err() {
                eprintln!("ui extensions query failed; skipping");
                return Ok(());
            }
            has_cell(&h.stdout().unwrap_or_default(), "1")
        };
        if !loaded {
            eprintln!("ui not embedded/loaded in this core; skipping");
            return Ok(());
        }
        let sql = "CALL start_ui();";
        let args = ["duckdb-cli", "-c", sql];
        let mut h = CliHarness::new(&args, &preopens)?;
        let status = h.run()?;
        let stdout = h.stdout().unwrap_or_default();
        let stderr = h.stderr().unwrap_or_default();
        if status.is_err() {
            panic!("start_ui() failed\nstdout:\n{stdout}\nstderr:\n{stderr}");
        }
        let low = stdout.to_lowercase();
        assert!(
            low.contains("ui started") || low.contains("localhost"),
            "start_ui() did not report a started server; got:\n{stdout}\nstderr:\n{stderr}"
        );
        Ok(())
    }

    #[test]
    fn smoke_runs_sql_against_disk_database() -> Result<()> {
        let tempdir = tempdir().context("failed to create temporary directory")?;
        let db_host_path = tempdir.path().join("smoke.db");
        // The tempdir is preopened at guest path ".", so the database lives at a
        // relative path inside it (an actual on-disk file, not :memory:).
        let db_guest_path = "smoke.db";

        // First process: create the database on disk and populate it.
        let write_cmd = "CREATE TABLE items(v INTEGER); \
                         INSERT INTO items VALUES (1), (2), (3);";
        let write_args = ["duckdb-cli", db_guest_path, "-c", write_cmd];
        let preopens = [(tempdir.path(), ".")];
        let mut writer = CliHarness::new(&write_args, &preopens)?;
        let write_status = writer.run()?;
        if write_status.is_err() {
            panic!(
                "writer CLI returned error status\nstdout:\n{}\nstderr:\n{}",
                writer.stdout().unwrap_or_default(),
                writer.stderr().unwrap_or_default()
            );
        }
        assert!(
            db_host_path.exists(),
            "expected on-disk database file to be created at {}",
            db_host_path.display()
        );

        // Second process: reopen the same file and read the data back, proving
        // the data persisted to disk across connections.
        let read_cmd = "SELECT SUM(v) AS total, COUNT(*) AS count FROM items;";
        let read_args = ["duckdb-cli", db_guest_path, "-c", read_cmd];
        let mut reader = CliHarness::new(&read_args, &preopens)?;
        let read_status = reader.run()?;
        if read_status.is_err() {
            panic!(
                "reader CLI returned error status\nstdout:\n{}\nstderr:\n{}",
                reader.stdout().unwrap_or_default(),
                reader.stderr().unwrap_or_default()
            );
        }

        let stdout = reader.stdout()?;
        assert!(
            has_cell(&stdout, "total") && has_cell(&stdout, "count"),
            "expected aggregated header in stdout, got:\n{stdout}"
        );
        assert!(
            has_cell(&stdout, "6") && has_cell(&stdout, "3"),
            "expected aggregated row in stdout, got:\n{stdout}"
        );

        Ok(())
    }

    #[test]
    fn cli_meta_commands_import_read_and_mode() -> Result<()> {
        let tempdir = tempdir().context("failed to create temporary directory")?;
        let preopens = [(tempdir.path(), ".")];

        std::fs::write(
            tempdir.path().join("people.csv"),
            "id,name\n1,alice\n2,bob\n3,carol\n",
        )?;
        // A script exercising .import (reads the CSV via the core fs shims) and a
        // trailing query, all run through .read.
        std::fs::write(
            tempdir.path().join("load.sql"),
            "CREATE TABLE people(id INTEGER, name TEXT);\n\
             .import people.csv people\n\
             SELECT count(*) AS n FROM people;\n",
        )?;
        let mut h = CliHarness::new(
            &["duckdb-cli", ":memory:", "-c", ".read load.sql"],
            &preopens,
        )?;
        assert!(h.run()?.is_ok(), ".read/.import failed: {}", h.stderr()?);
        let stdout = h.stdout()?;
        assert!(
            has_cell(&stdout, "3"),
            "expected imported row count 3, got:\n{stdout}"
        );

        // .mode csv switches the output format (no box borders).
        std::fs::write(
            tempdir.path().join("csv.sql"),
            ".mode csv\nSELECT 7 AS v, 'a,b' AS s;\n",
        )?;
        let mut csv = CliHarness::new(
            &["duckdb-cli", ":memory:", "-c", ".read csv.sql"],
            &preopens,
        )?;
        assert!(csv.run()?.is_ok(), ".mode csv failed: {}", csv.stderr()?);
        let csv_out = csv.stdout()?;
        assert!(
            csv_out.contains("v,s") && csv_out.contains("7,\"a,b\"") && !csv_out.contains('|'),
            "expected CSV output, got:\n{csv_out}"
        );

        // .mode json emits typed values: numbers/booleans unquoted, text quoted.
        std::fs::write(
            tempdir.path().join("json.sql"),
            ".mode json\nSELECT 1 AS i, true AS b, 'x' AS s;\n",
        )?;
        let mut json = CliHarness::new(
            &["duckdb-cli", ":memory:", "-c", ".read json.sql"],
            &preopens,
        )?;
        assert!(json.run()?.is_ok(), ".mode json failed: {}", json.stderr()?);
        let json_out = json.stdout()?;
        assert!(
            json_out.contains(r#"{"i":1,"b":true,"s":"x"}"#),
            "expected typed JSON (unquoted number/bool), got:\n{json_out}"
        );

        Ok(())
    }

    #[test]
    fn cli_output_redirects_to_file() -> Result<()> {
        let tempdir = tempdir().context("failed to create temporary directory")?;
        let preopens = [(tempdir.path(), ".")];

        // First query goes to the file; .output stdout restores stdout.
        std::fs::write(
            tempdir.path().join("redirect.sql"),
            ".output captured.txt\n\
             SELECT 7 AS answer;\n\
             .output stdout\n\
             SELECT 'on stdout' AS where_am_i;\n",
        )?;
        let mut h = CliHarness::new(
            &["duckdb-cli", ":memory:", "-c", ".read redirect.sql"],
            &preopens,
        )?;
        assert!(h.run()?.is_ok(), ".output failed: {}", h.stderr()?);

        let stdout = h.stdout()?;
        assert!(
            has_cell(&stdout, "on stdout") && !has_cell(&stdout, "7"),
            "post-redirect query should be on stdout, the redirected one should not:\n{stdout}"
        );
        let file = std::fs::read_to_string(tempdir.path().join("captured.txt"))?;
        assert!(
            has_cell(&file, "answer") && has_cell(&file, "7"),
            "redirected query should be in the file, got:\n{file}"
        );

        Ok(())
    }

    #[test]
    fn cli_meta_commands_introspect_schema() -> Result<()> {
        let tempdir = tempdir().context("failed to create temporary directory")?;
        let preopens = [(tempdir.path(), ".")];
        let db = "meta.db";

        // Create schema on disk in one process.
        let mut writer = CliHarness::new(
            &[
                "duckdb-cli",
                db,
                "-c",
                "CREATE TABLE widgets(id INTEGER PRIMARY KEY, label TEXT); \
                 CREATE INDEX idx_label ON widgets(label); \
                 CREATE TABLE gadgets(id INTEGER);",
            ],
            &preopens,
        )?;
        assert!(writer.run()?.is_ok(), "writer failed: {}", writer.stderr()?);

        // .tables lists both tables.
        let mut tables = CliHarness::new(&["duckdb-cli", db, "-c", ".tables"], &preopens)?;
        assert!(tables.run()?.is_ok(), "`.tables` failed: {}", tables.stderr()?);
        let tables_out = tables.stdout()?;
        assert!(
            has_cell(&tables_out, "widgets") && has_cell(&tables_out, "gadgets"),
            "expected both tables in `.tables`, got:\n{tables_out}"
        );

        // .schema shows the CREATE statement for a specific table.
        let mut schema = CliHarness::new(&["duckdb-cli", db, "-c", ".schema widgets"], &preopens)?;
        assert!(schema.run()?.is_ok(), "`.schema` failed: {}", schema.stderr()?);
        let schema_out = schema.stdout()?;
        assert!(
            schema_out.contains("CREATE TABLE widgets"),
            "expected CREATE TABLE in `.schema widgets`, got:\n{schema_out}"
        );

        // .indexes lists the index.
        let mut indexes = CliHarness::new(&["duckdb-cli", db, "-c", ".indexes"], &preopens)?;
        assert!(indexes.run()?.is_ok(), "`.indexes` failed: {}", indexes.stderr()?);
        let indexes_out = indexes.stdout()?;
        assert!(
            has_cell(&indexes_out, "idx_label"),
            "expected idx_label in `.indexes`, got:\n{indexes_out}"
        );

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
    fn cli_function_prefix_qualified_form_works() -> Result<()> {
        // PLAN-prefixes killer demo (host smoke): a component scalar is callable
        // BOTH as bare `name(...)` and as `prefix__name(...)`. sample_extension
        // has no registry prefix/expansion, so the host's deprecation fallback
        // gives it prefix=`sample_extension`; both forms must return 42.
        ensure_sample_extension_artifact()?;

        let args = [
            "duckdb-cli",
            ":memory:",
            "--load-extension",
            "sample_extension",
            "-c",
            "select sample_plus_one(41) as bare, \
                    sample_extension__sample_plus_one(41) as qualified;",
        ];

        let mut harness = CliHarness::new(&args, &[])?;
        let status = harness.run()?;
        assert!(
            status.is_ok(),
            "CLI reported failure invoking the qualified form: {:?}",
            harness.stderr().ok()
        );

        let stdout = harness.stdout()?;
        assert!(
            has_cell(&stdout, "bare") && has_cell(&stdout, "qualified"),
            "expected both bare + qualified columns, got:\n{stdout}"
        );
        // Both columns hold 42 (the qualified form shares the same callback).
        assert!(
            stdout.matches("42").count() >= 2,
            "expected the qualified form to also return 42, got:\n{stdout}"
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
    fn cli_executes_sample_macro() -> Result<()> {
        ensure_sample_extension_artifact()?;

        let args = [
            "duckdb-cli",
            ":memory:",
            "--load-extension",
            "sample_extension",
            "-c",
            "select sample_add_two(40) as answer;",
        ];

        let mut harness = CliHarness::new(&args, &[])?;
        let status = harness.run()?;
        assert!(
            status.is_ok(),
            "CLI reported failure invoking sample_add_two macro: {:?}",
            harness.stderr().ok()
        );

        let stdout = harness.stdout()?;
        assert!(
            has_cell(&stdout, "answer") && has_cell(&stdout, "42"),
            "expected macro output, got:\n{}",
            stdout
        );

        Ok(())
    }

    #[test]
    fn cli_executes_replacement_scan() -> Result<()> {
        ensure_sample_extension_artifact()?;

        let args = [
            "duckdb-cli",
            ":memory:",
            "--load-extension",
            "sample_extension",
            "-c",
            "select * from 'hello.sample';",
        ];

        let mut harness = CliHarness::new(&args, &[])?;
        let status = harness.run()?;
        assert!(
            status.is_ok(),
            "CLI reported failure running replacement scan: {:?}",
            harness.stderr().ok()
        );

        let stdout = harness.stdout()?;
        assert!(
            has_cell(&stdout, "hello.sample"),
            "expected replacement-scan output, got:\n{}",
            stdout
        );

        Ok(())
    }

    #[test]
    fn cli_uses_registered_logical_type() -> Result<()> {
        ensure_sample_extension_artifact()?;

        let args = [
            "duckdb-cli",
            ":memory:",
            "--load-extension",
            "sample_extension",
            "-c",
            "select 7::sample_id as v;",
        ];

        let mut harness = CliHarness::new(&args, &[])?;
        let status = harness.run()?;
        assert!(
            status.is_ok(),
            "CLI reported failure casting to registered logical type: {:?}",
            harness.stderr().ok()
        );

        let stdout = harness.stdout()?;
        assert!(
            has_cell(&stdout, "v") && has_cell(&stdout, "7"),
            "expected logical-type cast output, got:\n{}",
            stdout
        );

        Ok(())
    }

    #[test]
    fn cli_invokes_registered_cast() -> Result<()> {
        ensure_sample_extension_artifact()?;

        // The built-in VARCHAR->integer cast fails on "id-7"; a 7 here proves the
        // extension's custom cast callback ran.
        let args = [
            "duckdb-cli",
            ":memory:",
            "--load-extension",
            "sample_extension",
            "-c",
            "select cast('id-7' as sample_id) as v;",
        ];

        let mut harness = CliHarness::new(&args, &[])?;
        let status = harness.run()?;
        assert!(
            status.is_ok(),
            "CLI reported failure invoking custom cast: {:?}",
            harness.stderr().ok()
        );

        let stdout = harness.stdout()?;
        assert!(
            has_cell(&stdout, "v") && has_cell(&stdout, "7"),
            "expected custom cast output, got:\n{}",
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
        extension_catalog::add_to_linker::<TestExtensionHost, TestExtensionHost>(
            &mut linker,
            |state| state,
        )?;
        extension_files::add_to_linker::<TestExtensionHost, TestExtensionHost>(
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

    impl extension_runtime::HostCastCallback for TestExtensionHost {
        fn new(
            &mut self,
            _handle: u32,
        ) -> wasmtime::component::Resource<extension_runtime::CastCallback> {
            wasmtime::component::Resource::new_own(self.alloc_resource_id())
        }

        fn call(
            &mut self,
            _self_: wasmtime::component::Resource<extension_runtime::CastCallback>,
            _value: extension_types::Duckvalue,
        ) -> Result<extension_types::Duckvalue, extension_types::Duckerror> {
            Err(unsupported_runtime_error())
        }

        fn drop(
            &mut self,
            _rep: wasmtime::component::Resource<extension_runtime::CastCallback>,
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

    impl extension_catalog::Host for TestExtensionHost {
        fn register_logical_type(
            &mut self,
            _ty: extension_catalog::LogicalType,
        ) -> Result<u32, String> {
            Ok(0)
        }

        fn register_cast(
            &mut self,
            _spec: extension_catalog::CastSpec,
            _callback: wasmtime::component::Resource<extension_catalog::CastCallback>,
        ) -> Result<(), String> {
            Ok(())
        }

        fn register_macro(&mut self, _def: extension_catalog::MacroDef) -> Result<(), String> {
            Ok(())
        }
    }

    impl extension_files::Host for TestExtensionHost {
        fn register_replacement_scan(
            &mut self,
            _scan: extension_files::ReplacementScan,
        ) -> Result<u32, String> {
            Ok(0)
        }

        fn register_copy_handler(
            &mut self,
            _handler: extension_files::CopyHandler,
        ) -> Result<u32, String> {
            Ok(0)
        }
    }

    // ----------------------------------------------------------------------
    // Pure converter unit tests (no engine / no .wasm artifact). These cover
    // the neutral<->core / core<->cli / core<->extension value+type converters
    // on the dispatch hot path, including the rich (int8..timestamptz,
    // decimal/interval/uuid) and Complex escape-hatch arms.
    // ----------------------------------------------------------------------

    /// Every neutral logicaltype, including the rich set + a Complex expr.
    fn all_neutral_logicaltypes() -> Vec<reg::LogicalType> {
        use reg::LogicalType as L;
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
            L::Complex("LIST(INTEGER)".to_string()),
        ]
    }

    #[test]
    fn neutral_logicaltype_to_core_covers_every_arm() {
        // Every arm converts without panicking; the Complex arm carries its
        // owned type-expr through to the core variant.
        for ty in all_neutral_logicaltypes() {
            let is_complex = matches!(ty, reg::LogicalType::Complex(_));
            let core = neutral_logicaltype_to_core(ty);
            if is_complex {
                assert!(matches!(
                    core,
                    core_runtime_exports::Logicaltype::Complex(ref e) if e == "LIST(INTEGER)"
                ));
            }
        }
    }

    /// Construct a representative core duckvalue per arm (rich set included).
    fn all_core_duckvalues() -> Vec<core_types::Duckvalue> {
        use core_types::Duckvalue as C;
        vec![
            C::Null,
            C::Boolean(true),
            C::Int64(-5),
            C::Uint64(5),
            C::Float64(1.25),
            C::Text("t".into()),
            C::Blob(vec![9, 8, 7]),
            C::Int32(-3),
            C::Timestamp(11),
            C::Int8(-1),
            C::Int16(-2),
            C::Uint8(1),
            C::Uint16(2),
            C::Uint32(3),
            C::Float32(0.5),
            C::Date(100),
            C::Time(200),
            C::Timestamptz(300),
            C::Decimal(core_types::Decimalvalue {
                lower: 77,
                upper: 0,
                width: 6,
                scale: 3,
            }),
            C::Interval(core_types::Intervalvalue {
                months: 1,
                days: 2,
                micros: 3,
            }),
            C::Uuid(core_types::Uuidvalue { hi: 10, lo: 20 }),
            C::Complex(core_types::Complexvalue {
                type_expr: "STRUCT(a INT)".into(),
                json: "{\"a\":1}".into(),
            }),
        ]
    }

    #[test]
    fn core_cli_duckvalue_round_trips_every_arm() {
        // core -> cli -> core is lossless for every arm including the rich ones.
        for v in all_core_duckvalues() {
            let cli = convert_core_duckvalue(v.clone());
            let back = convert_cli_duckvalue(cli);
            // Compare via debug-format (the generated types don't derive PartialEq
            // uniformly, but their Debug is structural and stable).
            assert_eq!(format!("{v:?}"), format!("{back:?}"));
        }
    }

    #[test]
    fn core_extension_duckvalue_round_trips_every_arm() {
        // core -> extension -> core is lossless for every arm.
        for v in all_core_duckvalues() {
            let ext = convert_core_duckvalue_to_extension(v.clone());
            let back = convert_extension_duckvalue_to_core(ext);
            assert_eq!(format!("{v:?}"), format!("{back:?}"));
        }
    }

    #[test]
    fn core_storage_duckvalue_converts_every_arm() {
        // core -> storage scan value: every arm converts without panic; spot
        // check the rich arms preserve their payload.
        for v in all_core_duckvalues() {
            let s = convert_core_duckvalue_to_storage(v.clone());
            match (&v, &s) {
                (
                    core_types::Duckvalue::Decimal(d),
                    storage_scan::Duckvalue::Decimal(sd),
                ) => {
                    assert_eq!((d.lower, d.width, d.scale), (sd.lower, sd.width, sd.scale));
                }
                (
                    core_types::Duckvalue::Complex(c),
                    storage_scan::Duckvalue::Complex(sc),
                ) => {
                    assert_eq!(c.type_expr, sc.type_expr);
                    assert_eq!(c.json, sc.json);
                }
                _ => {}
            }
        }
    }

    #[test]
    fn neutral_funcflags_to_core_maps_each_bit() {
        let none = neutral_funcflags_to_core(reg::FuncFlags::default());
        assert_eq!(none, core_types::Funcflags::empty());
        let all = neutral_funcflags_to_core(reg::FuncFlags {
            deterministic: true,
            commutative: true,
            stateless: true,
            side_effecting: true,
            deprecated: true,
        });
        assert!(all.contains(core_types::Funcflags::DETERMINISTIC));
        assert!(all.contains(core_types::Funcflags::COMMUTATIVE));
        assert!(all.contains(core_types::Funcflags::STATELESS));
        assert!(all.contains(core_types::Funcflags::SIDEEFFECTING));
        assert!(all.contains(core_types::Funcflags::DEPRECATED));
    }

    #[test]
    fn describe_core_duckvalue_is_total_and_nonempty() {
        // The dispatch-path describe helper must handle every arm (a component
        // returning any variant) without panicking and yield a label.
        for v in all_core_duckvalues() {
            assert!(!describe_core_duckvalue(&v).is_empty());
        }
    }
}
fn resolve_preopens_with_default(preopens: &[(&Path, &str)]) -> Result<Vec<(PathBuf, String)>> {
    let mut merged = Vec::with_capacity(preopens.len() + 1);
    // Only fall back to the current directory when the caller hasn't already
    // mapped the guest cwd ("."). Otherwise the default would shadow an explicit
    // "." preopen — the core's path resolver keeps the first match for equal
    // scores, so files would be created in the host cwd instead of the caller's
    // directory.
    let caller_maps_cwd = preopens
        .iter()
        .any(|(_, guest)| *guest == "." || *guest == "./" || guest.is_empty());
    if !caller_maps_cwd {
        merged.push((std::env::current_dir()?, ".".to_string()));
    }
    for (host, guest) in preopens {
        merged.push((host.to_path_buf(), guest.to_string()));
    }
    Ok(merged)
}
