pub mod duckdb_core_bindings {
    wasmtime::component::bindgen!({
        path: "../../crates/ducklink-core/wit",
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
use duckdb_core_bindings::duckdb::extension::types as core_types;
use duckdb_core_bindings::tvm::memory::bytes as core_tvm_bytes;
use duckdb_core_bindings::tvm::memory::manager as core_tvm_manager;
use duckdb_core_bindings::tvm::memory::types as core_tvm_types;
use duckdb_core_bindings::exports::duckdb::component::database as core_db_exports;
use duckdb_core_bindings::exports::duckdb::extension::{
    config as core_config_exports, logging as core_logging_exports, runtime as core_runtime_exports,
};
use ducklink_runtime::duckdb_extension_bindings::duckdb::extension::{
    catalog as extension_catalog, config as extension_config, files as extension_files,
    logging as extension_logging, runtime as extension_runtime, types as extension_types,
};
use ducklink_runtime::duckdb_extension_bindings::{DuckdbExtension, DuckdbExtensionPre};
use wasmtime::component::__internal::Vec as BindgenVec;
use ducklink_runtime::{CallbackEntry, CallbackKind, CallbackRegistry};
use wasmtime::component::{Component, Linker, Resource, ResourceAny, ResourceTable};
use wasmtime::{AsContextMut, Config, Engine, Store, StoreContextMut};

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

    #[allow(clippy::ptr_arg)] // the bindgen call takes &Vec (the rowbatch type), not a slice
    fn dispatch_scalar_batch(
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

    fn dispatch_cast(
        &mut self,
        dispatcher_handle: u32,
        value: &extension_types::Duckvalue,
    ) -> Result<extension_types::Duckvalue, extension_types::Duckerror> {
        let guest = self.bindings.duckdb_extension_callback_dispatch();
        let mut store = self.store.as_context_mut();
        guest
            .call_call_cast(&mut store, dispatcher_handle, value)
            .map_err(map_extension_trap)?
            .map_err(|err| err)
    }

    fn drain_pending(&mut self) -> PendingRegistrationsData {
        let mut ctx = self.store.as_context_mut();
        let data: *mut ExtensionStoreState = ctx.data_mut();
        unsafe { (*data).drain_pending() }
    }
}

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
}
impl WasiView for DotcmdState {
    fn ctx(&mut self) -> WasiCtxView<'_> {
        WasiCtxView { ctx: &mut self.wasi, table: &mut self.table }
    }
}
impl wasmtime::component::HasData for DotcmdState {
    type Data<'a> = &'a mut DotcmdState;
}

/// `duckdb:dotcmd/spi` — run SQL on the CLI's live connection, returned as
/// tab/newline-delimited text. Shares the user's connection (temp tables,
/// `:memory:` state, settings).
impl dotcmd_bindings::duckdb::dotcmd::spi::Host for DotcmdState {
    fn query(&mut self, sql: String) -> Result<String, String> {
        let handle = self
            .current_connection
            .lock()
            .expect("current connection mutex poisoned")
            .clone()
            .ok_or_else(|| "spi: no active database connection".to_string())?;
        let mut core = self.core.lock().expect("core mutex poisoned");
        let result = core
            .with_database(|guest, store| guest.call_execute(store, handle, &sql))
            .map_err(|trap| format!("spi query trapped: {trap}"))?;
        match result {
            Ok(qr) => Ok(spi_render_rows(qr)),
            Err(err) => Err(core_duckerror_message(err)),
        }
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
    }
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
            match Self::load_one(engine, &path, core.clone(), current_connection.clone()) {
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
    ) -> wasmtime::Result<(DotcmdInstance, Vec<(String, u64, String, String)>)> {
        let component = load_component(engine, path)?;
        let mut linker = Linker::<DotcmdState>::new(engine);
        p2::add_to_linker_sync(&mut linker)?;
        dotcmd_bindings::duckdb::dotcmd::spi::add_to_linker::<DotcmdState, DotcmdState>(
            &mut linker,
            |s| s,
        )?;
        let wasi = WasiCtxBuilder::new().inherit_stdio().build();
        let mut store = Store::new(
            engine,
            DotcmdState {
                wasi,
                table: ResourceTable::new(),
                core,
                current_connection,
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
    let registry = registry.lock().expect("dotcmd registry mutex poisoned");
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
            extension_catalog::add_to_linker::<ExtensionStoreState, ExtensionStoreState>(
                &mut linker,
                |state| state,
            )?;
            extension_files::add_to_linker::<ExtensionStoreState, ExtensionStoreState>(
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
    pending_macros: Vec<PendingMacro>,
    pending_replacement_scans: Vec<PendingReplacementScan>,
    pending_logical_types: Vec<PendingLogicalType>,
    pending_casts: Vec<PendingCast>,
    /// Maps the handle returned from `table-registry.register` to the table
    /// function name, so `files.register-replacement-scan` can resolve it.
    table_handle_names: HashMap<u32, String>,
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

#[derive(Default)]
struct PendingRegistrationsData {
    scalars: Vec<PendingScalar>,
    tables: Vec<PendingTable>,
    aggregates: Vec<PendingAggregate>,
    macros: Vec<PendingMacro>,
    replacement_scans: Vec<PendingReplacementScan>,
    logical_types: Vec<PendingLogicalType>,
    casts: Vec<PendingCast>,
}

impl PendingRegistrationsData {
    fn append(&mut self, mut other: PendingRegistrationsData) {
        self.scalars.append(&mut other.scalars);
        self.tables.append(&mut other.tables);
        self.aggregates.append(&mut other.aggregates);
        self.macros.append(&mut other.macros);
        self.replacement_scans.append(&mut other.replacement_scans);
        self.logical_types.append(&mut other.logical_types);
        self.casts.append(&mut other.casts);
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
            pending_macros: Vec::new(),
            pending_replacement_scans: Vec::new(),
            pending_logical_types: Vec::new(),
            pending_casts: Vec::new(),
            table_handle_names: HashMap::new(),
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
        let macros = std::mem::take(&mut self.pending_macros);
        let replacement_scans = std::mem::take(&mut self.pending_replacement_scans);
        let logical_types = std::mem::take(&mut self.pending_logical_types);
        let casts = std::mem::take(&mut self.pending_casts);
        let pending = PendingRegistrationsData {
            scalars,
            tables,
            aggregates,
            macros,
            replacement_scans,
            logical_types,
            casts,
        };
        let scalar_names = summarize_registration_names(&pending.scalars, |entry| entry.name.as_str());
        let table_names =
            summarize_registration_names(&pending.tables, |entry| entry.name.as_str());
        let aggregate_names = summarize_registration_names(&pending.aggregates, |entry| entry.name.as_str());
        let macro_names = summarize_registration_names(&pending.macros, |entry| entry.name.as_str());
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

impl extension_runtime::HostCastCallback for ExtensionStoreState {
    fn new(
        &mut self,
        handle: u32,
    ) -> wasmtime::component::Resource<extension_runtime::CastCallback> {
        let id = self.allocate_callback_handle(handle, CallbackKind::Cast);
        wasmtime::component::Resource::new_own(id)
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
        rep: wasmtime::component::Resource<extension_runtime::CastCallback>,
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

// The `catalog` and `files` interfaces are part of the extension world so that
// extensions can register logical types, casts, macros, replacement scans, and
// copy handlers. The host satisfies the imports here so such extensions
// instantiate and load; the requests are acknowledged and logged. Forwarding
// them into DuckDB (via new core hooks + C API calls) is a follow-up — see
// docs/PLAN-capability-migration.md.
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
        callback: wasmtime::component::Resource<extension_catalog::CastCallback>,
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
                    .expect("current connection mutex poisoned") = Some(handle.clone());
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
                    .expect("current connection mutex poisoned") = Some(handle.clone());
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

fn summarize_runtime_funcargs(args: &[reg::FuncArg]) -> String {
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

fn summarize_runtime_columns(columns: &[reg::ColumnDef]) -> String {
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

fn summarize_funcopts(options: Option<&reg::FuncOpts>) -> String {
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
            let attrs = opts.attributes.describe();
            format!("description='{description}', tags={tags}, attrs={attrs}")
        }
    }
}

fn summarize_extopts(options: Option<&reg::ExtOpts>) -> String {
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

fn describe_runtime_logicaltype(ty: &reg::LogicalType) -> &'static str {
    ty.describe()
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
            .with_context(|| format!("failed to deserialize precompiled {}", path.display()))
    } else {
        Component::from_file(engine, path)
            .with_context(|| format!("failed to load {}", path.display()))
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
        .with_context(|| format!("precompile {}", in_path.display()))?;
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
    // Grant outbound network so wasi:sockets-backed code (e.g. httpfs over the
    // linked openssl/mbedtls + wasi-libc BSD sockets) can connect + resolve DNS.
    builder.inherit_network();
    builder.allow_ip_name_lookup(true);
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
    builder.inherit_network();
    builder.allow_ip_name_lookup(true);
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

fn convert_extension_funcflags(flags: extension_types::Funcflags) -> reg::FuncFlags {
    reg::FuncFlags {
        deterministic: flags.contains(extension_types::Funcflags::DETERMINISTIC),
        commutative: flags.contains(extension_types::Funcflags::COMMUTATIVE),
        stateless: flags.contains(extension_types::Funcflags::STATELESS),
        side_effecting: flags.contains(extension_types::Funcflags::SIDEEFFECTING),
        deprecated: flags.contains(extension_types::Funcflags::DEPRECATED),
    }
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
        let current_connection = Arc::new(Mutex::new(None));
        let dotcmd_registry = Arc::new(Mutex::new(DotcmdRegistry::load(
            &engine,
            &dotcmd_root(),
            core.clone(),
            current_connection.clone(),
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
                let mut registry = registry.lock().expect("dotcmd registry mutex poisoned");
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
    let current_connection = Arc::new(Mutex::new(None));
    let dotcmd_registry = Arc::new(Mutex::new(DotcmdRegistry::load(
        &engine,
        &dotcmd_root(),
        core.clone(),
        current_connection.clone(),
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
            let mut registry = registry.lock().expect("dotcmd registry mutex poisoned");
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
