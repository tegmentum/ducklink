pub mod duckdb_core_bindings {
    wasmtime::component::bindgen!({
        // Phase 2: consume the @5 core world from ducklink's synced mirror
        // (populated by scripts/sync-cli-wit.sh). No longer references the
        // out-of-tree duckdb-wasm working copy.
        path: "../../crates/ducklink-cli/wit/deps/duckdb",
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

// Module declared here (rather than with the other `pub mod` items further
// down) so `driver_tool_bindings` below can reference
// `crate::driver_exec::DriverConnection` in its `with:` map.
pub mod driver_exec;

/// Generated bindings for the cron-driver-tool world.
///
/// The tool is a `wasi:cli/run` command component that imports our small
/// `duckdb:driver/exec` bridge (open/exec/query) plus a handful of standard
/// WASI-p2 interfaces (environment/stdout/stderr, monotonic-clock,
/// wall-clock, io/poll, io/streams). The `with:` map wires every WASI
/// interface to the wasmtime-wasi types so `p2::add_to_linker_sync` can
/// service them, and maps the driver-exec `connection` resource to the
/// native `DriverConnection` struct so the ResourceTable stores it
/// directly. Only the `Host`/`HostConnection` traits are left generated;
/// `driver_exec.rs` implements them on `DriverStoreState`.
pub mod driver_tool_bindings {
    wasmtime::component::bindgen!({
        path: "../../extensions/cron-driver-tool/wit",
        world: "duckdb:driver-tool/cron-driver-tool",
        with: {
            "wasi:cli/environment": wasmtime_wasi::p2::bindings::cli::environment,
            "wasi:cli/stdout": wasmtime_wasi::p2::bindings::cli::stdout,
            "wasi:cli/stderr": wasmtime_wasi::p2::bindings::cli::stderr,
            "wasi:clocks/monotonic-clock": wasmtime_wasi::p2::bindings::clocks::monotonic_clock,
            "wasi:clocks/wall-clock": wasmtime_wasi::p2::bindings::clocks::wall_clock,
            "wasi:io/poll": wasmtime_wasi::p2::bindings::io::poll,
            "wasi:io/streams": wasmtime_wasi::p2::bindings::io::streams,
            "wasi:io/error": wasmtime_wasi::p2::bindings::io::error,
            "duckdb:driver/exec.connection": crate::driver_exec::DriverConnection,
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
use std::sync::{Arc, Mutex, OnceLock, RwLock};
use std::thread;

use anyhow::{Context, Result};
use duckdb_cli_bindings::duckdb::component::database as cli_db;
use duckdb_cli_bindings::duckdb::extension::types as cli_types;
use duckdb_core_bindings::duckdb::component::extension_loader_hooks as core_extension_hooks;
use duckdb_core_bindings::duckdb::component::host_extension_loader as core_host_loader;
use duckdb_core_bindings::duckdb::extension::callback_dispatch as core_callback_dispatch;
use duckdb_core_bindings::duckdb::extension::column_types as core_column_types;
// Phase 2 (@5): the 8 `*-host` imports on the core WIT world are DELETED --
// storage/index/collation/pragma/parser/optimizer/files/table-stream all lift
// to the host, which orchestrates each per-extension `*-dispatch` export
// directly (see the ATTACH intercept + write intercept in HostState::execute
// and ADR wasm-ecosystem-at-5-adr.md Decision 3 + Amendment A1).
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
    ConfigError, ExtensionInstance, ExtensionServices, LogField, LogLevel, NestedExecResult,
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
pub mod at5_intercept;
mod delta_rewrite;
mod plan_shape;
/// Phase D: per-sub-extension `compose:dynlink` bridge + composed-provider
/// loader (`postgis_core -> {plan, bridge, derived-from}` maps and the
/// `materialize_sub_ext_provider` composer). Public so downstream host
/// embedders (and this crate's tests) can configure it directly instead of
/// going through env vars.
pub mod sub_ext;
pub use sub_ext::{sub_ext_provider_id, SubExtError, SubExtLoader};

/// Defensive guard for a parser extension's returned rewrite (v3 @3.0.0
/// parser-dispatch boundary). The core RE-PLANS the rewrite, so a bad component
/// must not be able to drive the core into a re-plan loop or hand it nothing to
/// parse. Rejects:
///   * an empty / whitespace-only rewrite (nothing to re-plan), and
///   * a rewrite byte-identical to the input statement (the simplest infinite
///     re-plan: the core re-offers the same text to us forever).
/// The full SQL is still validated by the core's binder; this only stops the
/// pathological shapes that never reach a clean binder error. Pure + panic-free.
fn validate_parser_rewrite(ext: &str, query: &str, rewrite: &str) -> Result<(), String> {
    let r = rewrite.trim();
    if r.is_empty() {
        return Err(format!("parser extension '{ext}' returned an empty rewrite"));
    }
    if r == query.trim() {
        return Err(format!(
            "parser extension '{ext}' returned a rewrite identical to the input (would re-loop)"
        ));
    }
    Ok(())
}
pub mod resolver;

/// `ducklink extension <subcommand>` (alias `ext`) — extension-management CLI UX.
pub mod extcli;
pub mod cron_cli;
// `pub mod driver_exec;` is declared at the top of this file (before the
// `driver_tool_bindings` bindgen expansion that references its types).
mod ui_server;

/// Sentinel callback handles for the resolver observability scalars
/// (`extension_provider` / `set_extension_provider`). The shell-glue registers
/// these two scalars with these exact handles; `dispatch_scalar_batch` routes
/// them to the resolver instead of a resident extension. Chosen at the top of
/// the u32 space so they never collide with real per-extension callback handles
/// (which start at 1 and increment).
pub const RESOLVER_EXPLAIN_HANDLE: u32 = 0xFFFF_FFFF;
pub const RESOLVER_SET_HANDLE: u32 = 0xFFFF_FFFE;

/// Sentinel callback handle for the `ducklink_load(name [, kind])` table
/// function committed in ducklink-extension's `STABILITY.md § 1.1`.
///
/// The workspace host has no non-component route from SQL into its extension
/// load orchestration: loading happens only on `LOAD <name>;` via the wasm
/// DuckDB core's `host-extension-loader` import, and SQL callables in the
/// current WIT surface have no way to trigger a peer LOAD without a
/// contract-breaking new import. `drain_pending_registrations` therefore
/// injects a synthetic [`reg::TableReg`] whose `callback_handle` is this
/// sentinel and [`ExtensionManager::dispatch_table`] intercepts the sentinel
/// to run the load orchestration natively — no wasm component backs it.
///
/// Follows the [`RESOLVER_EXPLAIN_HANDLE`] / [`RESOLVER_SET_HANDLE`] pattern
/// used for the resolver observability scalars: a well-known callback handle
/// the core registers under a chosen function name; the host handles
/// dispatch instead of routing to a resident extension.
pub const DUCKLINK_LOAD_HANDLE: u32 = 0xFFFF_FFFD;

/// Sentinel callback handles for the `ducklink_prefix(alias, namespace)`
/// entry points committed in ducklink-extension's `STABILITY.md § 1.1`.
///
/// Same rationale as [`DUCKLINK_LOAD_HANDLE`]: the workspace host has no
/// way for a wasm component to synthesize new `CREATE OR REPLACE MACRO`
/// DDL against the connection that called it (a callback runs inside the
/// core wasm store mid-call — re-entering `call_execute` would deadlock
/// the core mutex and violate wasmtime store re-entrancy). So
/// `drain_pending_registrations` injects synthetic [`reg::TableReg`] +
/// [`reg::ScalarReg`] entries under these sentinels and
/// [`ExtensionManager::dispatch_table`] / `dispatch_scalar[_batch]`
/// intercept them to run natively.
///
/// The native handlers cannot themselves re-enter the core to emit the
/// per-namespace `CREATE MACRO` shapes (same wasm store re-entrancy
/// constraint as [`native_ducklink_load`]), so they only VALIDATE the
/// (alias, namespace) pair and stash it in
/// `ExtensionManager::deferred_prefix_declarations`. On the user's next
/// `HostState::execute` boundary the stash is drained and the actual DDL
/// (`duckdb_functions()` scan, `CREATE OR REPLACE MACRO <alias>.<name>`
/// per function, `INSERT OR REPLACE INTO ducklink.prefixes`) runs on the
/// then-idle core.
pub const DUCKLINK_PREFIX_TABLE_HANDLE: u32 = 0xFFFF_FFFC;
pub const DUCKLINK_PREFIX_SCALAR_HANDLE: u32 = 0xFFFF_FFFB;

/// Phase 2c (@5): reserved handle range for host-owned table functions
/// synthesised by the ATTACH intercept. Each `<alias>.<table>` pair grafted
/// by `HostState::intercept_attach` allocates a fresh handle in this range
/// and registers `(extension, catalog_handle, table)` with the manager. When
/// the core fires `call-table(handle, args)` for the synthesised
/// `__<alias>_<table>()` name, `ExtensionManager::dispatch_table` recognises
/// the handle and routes to the storage-dispatch scan path instead of a
/// component-side `call-table`.
///
/// Kept well clear of the small-int extension callback space (which grows
/// from 1) and the five named top-of-u32 sentinels above. `0xFFC0_0000..=
/// 0xFFFE_FFFF` gives ~4 million distinct AT5 table-fn callbacks.
pub const AT5_ATTACH_SCAN_HANDLE_MIN: u32 = 0xFFC0_0000;
pub const AT5_ATTACH_SCAN_HANDLE_MAX: u32 = 0xFFFE_FFFF;

pub use ui_server::{serve_ui, UiMode};
mod quack_server;
pub use quack_server::serve_quack;
mod handler;
pub use handler::HandlerRegistry;
mod httpd;
pub use httpd::{serve_httpd, HttpdOptions, TlsMode};
// duckstream MVP: SigV4 signing (reused verbatim from the wasm s3fs transport)
// + the native-host checkpoint-snapshot replicator/restore.
mod sigv4;
mod replicate;
pub use replicate::{run_backup, run_restore, ReplicaState, S3Target};
// `ducklink publish`: upload the catalog + content-addressed artifacts to the
// shared Cloudflare R2 extension-distribution bucket (reuses `sigv4`).
pub mod publish;
pub use publish::{plan_publish, print_dry_run, run_publish, PlanInputs, PublishPlan};
use wasmtime_wasi::p2::{
    self,
    pipe::{MemoryInputPipe, MemoryOutputPipe},
};
use wasmtime_wasi::{DirPerms, FilePerms, WasiCtx, WasiCtxBuilder, WasiCtxView, WasiView};
// wasi:http/{types,outgoing-handler}@0.2.9 host wiring. The composed
// cache.wasm imports wasi:http via the s3-wasm HTTPS transport (commit
// d2b8870); every store type this host builds a linker for must (a) impl
// WasiHttpView and (b) call `add_only_http_to_linker_sync` alongside the
// existing `p2::add_to_linker_sync`. Using `add_only_http_to_linker_sync`
// (not the full `add_to_linker_sync`) avoids re-adding wasi:cli/filesystem/
// etc, which would clash with the wasmtime_wasi call.
use wasmtime_wasi_http::p2::{
    add_only_http_to_linker_sync as add_wasi_http_to_linker, WasiHttpCtxView, WasiHttpView,
};
use wasmtime_wasi_http::WasiHttpCtx;

type CliString = wasmtime::component::__internal::String;

struct CoreStoreState {
    table: ResourceTable,
    wasi: WasiCtx,
    /// wasi:http host context (see the module-level `add_wasi_http_to_linker`
    /// import). Unused today by the core component itself; kept here so the
    /// `WasiHttpView` impl below has a store-owned `WasiHttpCtx` to project.
    wasi_http: WasiHttpCtx,
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
    /// Phase 4 follow-up (FU4): shared archive of every registration the
    /// PRIMARY's post-LOAD drain has emitted. Populated on the primary side by
    /// `impl core_extension_hooks::Host::get_pending_registrations` (append a
    /// clone before returning the drained value); read on the SIBLING side by
    /// the same trait method, which serves the archive instead of a drain so
    /// the sibling's DuckDB catalog acquires the same UDFs the primary did.
    ///
    /// `None` on stores that neither write to nor read from the archive (the
    /// narrow test paths that never opt into a `SiblingState`). See
    /// `SiblingState::replay_archive` for the shared Arc identity.
    replay_archive: Option<Arc<Mutex<ducklink_runtime::PendingRegistrationsData>>>,
    /// Phase 4 follow-up (FU4): true iff this store belongs to a lazy sibling
    /// [`CoreExecution`] built by [`sibling_ensure_slot`]. Sibling stores serve
    /// registrations from `replay_archive` on `get_pending_registrations`;
    /// primary stores drain the shared [`ExtensionManager`] as before and
    /// append the drained batch into `replay_archive` (when present).
    is_sibling: bool,
}

impl WasiView for CoreStoreState {
    fn ctx(&mut self) -> WasiCtxView<'_> {
        WasiCtxView {
            ctx: &mut self.wasi,
            table: &mut self.table,
        }
    }
}

impl WasiHttpView for CoreStoreState {
    fn http(&mut self) -> WasiHttpCtxView<'_> {
        WasiHttpCtxView {
            ctx: &mut self.wasi_http,
            table: &mut self.table,
            hooks: Default::default(),
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
    /// Phase 4 follow-up (FU4): serve the sibling core's post-LOAD
    /// registrations from the shared archive so extension-registered scalars
    /// / tables / aggregates reach the sibling's DuckDB catalog.
    ///
    /// * PRIMARY stores (`is_sibling == false`) drain the shared
    ///   [`ExtensionManager`] as before AND append a clone of the drained batch
    ///   into `replay_archive` (when wired) so a later sibling boot can replay
    ///   the same registrations without re-invoking each extension's `load()`.
    /// * SIBLING stores (`is_sibling == true`) serve `replay_archive` verbatim
    ///   and never touch the extension manager's own drain buffers — the
    ///   primary already consumed them, and re-draining would return nothing.
    ///
    /// Falls back to the historical drain-only behaviour when `replay_archive`
    /// is `None` (narrow test paths that never opt into a `SiblingState`).
    fn get_pending_registrations(&mut self) -> core_extension_hooks::PendingRegistrations {
        if self.is_sibling {
            // Sibling read path: clone-out the archive under its own mutex so
            // this call never touches the shared `ExtensionManager` mutex.
            // Avoids deadlock when the sibling is materialized from inside a
            // primary extension load thread that already holds the manager
            // mutex (see the shared-manager deadlock boundary in ADR
            // Amendment A5 Risk 5 + the sibling_ensure_slot comment block).
            if let Some(archive) = self.replay_archive.as_ref() {
                let guard = archive.lock().unwrap_or_else(|e| e.into_inner());
                return convert_pending_registrations(guard.clone());
            }
            return convert_pending_registrations(
                ducklink_runtime::PendingRegistrationsData::default(),
            );
        }
        // Primary write path: drain the shared manager, THEN append a snapshot
        // into the archive. Locks are acquired in order (manager → archive)
        // and released before the wit-bindgen conversion so any downstream
        // side-effect can't re-enter either mutex through a callback.
        let drained = {
            let mut manager = self
                .extension_manager
                .lock()
                .expect("extension manager mutex poisoned");
            manager.drain_pending_registrations()
        };
        if let Some(archive) = self.replay_archive.as_ref() {
            let mut guard = archive.lock().unwrap_or_else(|e| e.into_inner());
            guard.append(drained.clone());
        }
        convert_pending_registrations(drained)
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

    fn call_scalar_batch_col(
        &mut self,
        handle: u32,
        args: BindgenVec<core_callback_dispatch::Colvec>,
        ctx: core_callback_dispatch::Invokeinfo,
    ) -> Result<core_callback_dispatch::Colvec, core_types::Duckerror> {
        // major-4 columnar: the core hands one colvec per arg; pivot to the
        // extension manager's row-major dispatch (which re-pivots to the
        // extension's colvec ABI), then rebuild the result column.
        let ext_rows = core_colvecs_to_ext_rows(&args);
        let converted_ctx = convert_core_invokeinfo(ctx);
        let mut manager = self
            .extension_manager
            .lock()
            .expect("extension manager mutex poisoned");
        manager
            .dispatch_scalar_batch(handle, &ext_rows, converted_ctx)
            .map(ext_values_to_core_colvec)
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

    fn call_aggregate_col(
        &mut self,
        handle: u32,
        args: BindgenVec<core_callback_dispatch::Colvec>,
    ) -> Result<core_types::Duckvalue, core_types::Duckerror> {
        // major-4 columnar aggregate: pivot the buffered group columns to rows.
        let ext_rows = core_colvecs_to_ext_rows(&args);
        let mut manager = self
            .extension_manager
            .lock()
            .expect("extension manager mutex poisoned");
        manager
            .dispatch_aggregate(handle, &ext_rows)
            .map(convert_extension_duckvalue_to_core)
            .map_err(convert_extension_duckerror_to_core)
    }

    fn call_cast_col(
        &mut self,
        handle: u32,
        arg: core_callback_dispatch::Colvec,
    ) -> Result<core_callback_dispatch::Colvec, core_types::Duckerror> {
        // major-4 columnar cast: cast each row via the row-major dispatch_cast.
        let ext_rows = core_colvecs_to_ext_rows(std::slice::from_ref(&arg));
        let mut manager = self
            .extension_manager
            .lock()
            .expect("extension manager mutex poisoned");
        let mut out = Vec::with_capacity(ext_rows.len());
        for row in &ext_rows {
            let v = row
                .first()
                .cloned()
                .unwrap_or(extension_types::Duckvalue::Null);
            out.push(
                manager
                    .dispatch_cast(handle, &v)
                    .map_err(convert_extension_duckerror_to_core)?,
            );
        }
        Ok(ext_values_to_core_colvec(out))
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

// Phase 2 (@5): the eight `*-host::Host` trait implementations that used to
// bridge storage / index / collation / pragma / parser / optimizer /
// files / table-stream calls out of the wasm core have been DELETED. Those
// capabilities now flow through the ATTACH intercept + write intercept in
// HostState::execute (see ADR Decision 3 + Amendment A1). The dispatch
// plumbing in ExtensionInstance::storage_* and ExtensionManager::dispatch_*
// stays intact -- what changed is the CALLER: the host, not the core.

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

/// The `nested-exec` Direction-1 §5.(b.1) sibling-core state, shared between
/// [`HostState`] (which records the primary's opened DB path) and every
/// [`CoreServices`] (which lazily instantiates a second [`CoreExecution`] over
/// the same DB on first `nested_exec`).
///
/// The sibling runs in its own [`wasmtime::Store`] with a fresh
/// [`ExtensionManager`] and an idle mutex, so a `nested-exec` from inside an
/// outer statement's callback does NOT re-enter the primary core's store or
/// take the primary's contended mutex. See `nested-exec-direction-1-plan.md`
/// §5.(b.1).
///
/// **Phase 4 (2026-07-26).** When `extension_manager` is `Some`, the sibling's
/// [`CoreStoreState`] is wired to the PRIMARY's [`ExtensionManager`] instead of
/// a fresh one — so `nested-exec` SQL run through the sibling can dispatch
/// against the extensions the primary has loaded (their [`ExtensionInstance`]
/// map + shared callback registry). When `None` (narrow test paths that never
/// opt in), the sibling falls back to the historical fresh-manager behaviour;
/// [`CoreServices::nested_exec`] then prepends
/// [`NESTED_EXEC_DIRECTION2_REDIRECT`] on catalog-miss errors as before.
///
/// See ADR Decision 6 (`docs/wasm-ecosystem-at-5-adr.md`) + Amendment A5 Risk 5
/// for the design rationale + the deadlock-risk boundary (a sibling-side
/// callback firing while the primary is mid-callback would try to reacquire the
/// shared `Mutex<ExtensionManager>` — safe for pure-DML sibling SQL, unsafe if
/// the sibling SQL itself invokes an extension scalar).
struct SiblingState {
    engine: Engine,
    core_component_path: PathBuf,
    /// Preopens to grant the sibling's WASI ctx. The primary resolves user
    /// paths (`open "/data/foo.duckdb"`) against a preopen set, so the sibling
    /// must have the SAME set to reach the same file. Threaded here from
    /// `HostState`'s construction — one snapshot for the whole process.
    preopens: Vec<(PathBuf, String)>,
    /// The DB path the primary core opened.
    ///
    /// * Outer `None` — no `open` yet (nested-exec has nothing to sibling into).
    /// * `Some(None)` — in-memory database. Sibling cannot share it, so
    ///   `nested-exec` returns a clear error.
    /// * `Some(Some(path))` — file-backed database; sibling opens the same path.
    primary_db_path: Mutex<Option<Option<String>>>,
    /// Phase 4: the PRIMARY's [`ExtensionManager`] shared with the sibling.
    /// `Some` in the CLI / harness paths so the sibling's dispatch surface
    /// mirrors the primary's; `None` in narrow tests (backward-compatible with
    /// the historical fresh-manager behaviour). See the struct-level doc for
    /// the un-block criteria + risk boundary.
    extension_manager: Option<Arc<Mutex<ExtensionManager>>>,
    /// Phase 4 follow-up (FU4): shared archive of every registration the
    /// primary's post-LOAD drain has emitted. Clone of this Arc is threaded
    /// into BOTH the primary's [`CoreStoreState`] (write path — appended on
    /// every drain) and the sibling's [`CoreStoreState`] (read path — served
    /// verbatim on the sibling's post-LOAD `get_pending_registrations`).
    ///
    /// A dedicated `Mutex<_>` (separate from the shared `ExtensionManager`
    /// mutex) so the sibling's read path never has to lock the manager. That
    /// matters when the sibling is materialized from inside a primary
    /// `ensure_extension_loaded` load thread — the primary thread is holding
    /// the manager mutex across the load, so a sibling-side manager lock
    /// would deadlock (ADR Amendment A5 Risk 5).
    replay_archive: Arc<Mutex<ducklink_runtime::PendingRegistrationsData>>,
    /// Phase 4 follow-up (FU4): names of extensions the primary's
    /// [`ExtensionManager`] has fully loaded, kept here so
    /// [`sibling_ensure_slot`] can decide which `LOAD <name>` to issue on the
    /// sibling to trigger the wasm shim's `get_pending_registrations` call
    /// (which serves the archive above). Populated by
    /// [`ExtensionManager::ensure_extension_loaded`] after a successful
    /// `self.extensions.insert(...)`.
    ///
    /// A separate `Mutex<Vec<String>>` (rather than snooping
    /// `manager.extensions.keys()`) is required for the same deadlock reason
    /// as `replay_archive`.
    loaded_extensions: Mutex<Vec<String>>,
    /// Lazily-materialized second core executor + connection into it, cached
    /// for the process lifetime after first successful `nested_exec`.
    slot: Mutex<Option<SiblingSlot>>,
}

impl SiblingState {
    fn new(
        engine: Engine,
        core_component_path: PathBuf,
        preopens: Vec<(PathBuf, String)>,
        extension_manager: Option<Arc<Mutex<ExtensionManager>>>,
    ) -> Self {
        Self {
            engine,
            core_component_path,
            preopens,
            primary_db_path: Mutex::new(None),
            extension_manager,
            replay_archive: Arc::new(Mutex::new(
                ducklink_runtime::PendingRegistrationsData::default(),
            )),
            loaded_extensions: Mutex::new(Vec::new()),
            slot: Mutex::new(None),
        }
    }

    /// Record what the primary just opened. Called from `HostState::open` /
    /// `open_with_config`. `path == None` means the primary opened `:memory:`.
    fn record_primary_open(&self, path: Option<String>) {
        *self
            .primary_db_path
            .lock()
            .unwrap_or_else(|e| e.into_inner()) = Some(path);
    }

    /// Phase 4 follow-up (FU4): record that the primary just fully loaded
    /// `name`, so [`sibling_ensure_slot`] can later issue a `LOAD <name>` on
    /// the sibling to trigger its wasm shim to pull the replay archive.
    /// Idempotent — a repeated LOAD of the same extension only records it
    /// once.
    fn record_extension_loaded(&self, name: &str) {
        let mut list = self
            .loaded_extensions
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        if !list.iter().any(|n| n == name) {
            list.push(name.to_string());
        }
    }
}

/// The lazy-init cache: a second [`CoreExecution`] on the same underlying
/// DuckDB file + a connection resource opened inside its store. Both are
/// created together on the first `nested_exec` call and reused for every
/// subsequent call for the process lifetime.
struct SiblingSlot {
    core: Arc<Mutex<CoreExecution>>,
    connection: ResourceAny,
}

/// Normalize whatever the primary passed to `open` into the form the sibling
/// will use to open the same DB. `None` (the WIT default) OR the literal
/// `":memory:"` collapse to `None`, which [`SiblingState`] treats as
/// "in-memory, cannot share". Any other string is passed through verbatim as
/// the file path.
fn sanitize_sibling_open_path(primary: Option<&str>) -> Option<String> {
    match primary {
        None => None,
        Some(p) if p == ":memory:" || p.trim().is_empty() => None,
        Some(p) => Some(p.to_string()),
    }
}

/// Prefix prepended to a sibling `call_execute` error when the failure looks
/// like it references a function only a host-loaded extension would provide
/// (e.g. `Catalog Error: Scalar Function with name X does not exist!`). Points
/// the caller at the Direction-2 native ducklink extension, which shares the
/// primary connection and therefore sees every loaded extension. See
/// [`is_extension_related_error`] for the heuristic that gates it.
const NESTED_EXEC_DIRECTION2_REDIRECT: &str =
    "nested-exec (Direction 1): the sibling core does not have host extensions loaded.\n\
If this entry references a function provided by a loaded extension, run it under\n\
the native ducklink DuckDB extension (Direction 2) instead. Underlying error: ";

/// True if `msg` looks like DuckDB is complaining about a missing catalog entry
/// that a loaded extension would have provided — the failure shape a Direction-1
/// sibling produces for extension-touching SQL because it never LOADed those
/// extensions. Best-effort textual match; DuckDB does not expose a structured
/// "which extension owns this function" hint, so we recognise the two dominant
/// error shapes:
///
/// * `Catalog Error: <Kind> Function with name <name> does not exist` — the
///   generic missing-function message DuckDB emits when the catalog has no
///   entry for the identifier and no extension autoload matches.
/// * Messages that mention an extension explicitly (`Missing Extension Error`,
///   `... requires the ... extension`, `extension ... not loaded`).
///
/// Syntax errors, table-not-found, and other non-extension failures pass the
/// filter and surface verbatim (no misleading redirect).
fn is_extension_related_error(msg: &str) -> bool {
    let lower = msg.to_ascii_lowercase();
    // "Catalog Error: [Scalar|Table|Aggregate|Macro] Function with name X does not exist"
    // (also matches DuckDB's plain `Function with name X does not exist!` shape).
    if lower.contains("function with name") && lower.contains("does not exist") {
        return true;
    }
    if lower.contains("does not exist") && lower.contains("function") {
        return true;
    }
    // "Missing Extension Error" / "extension X is not loaded" / similar.
    if lower.contains("missing extension") {
        return true;
    }
    if lower.contains("extension")
        && (lower.contains("not loaded")
            || lower.contains("not installed")
            || lower.contains("not found"))
    {
        return true;
    }
    // "No function matches ... signature ..." — the binder-side complaint DuckDB
    // emits when a name resolves but no overload matches; commonly triggered when
    // an extension-registered overload is the one the user meant.
    if lower.contains("no function matches") {
        return true;
    }
    false
}

// ---------------------------------------------------------------------------
// nested-exec Direction-1 §7.8 option (a): primary-core re-entry TLS
// ---------------------------------------------------------------------------
//
// tests/reentrancy_poc.rs proved wasmtime permits a host callback to
// re-enter the SAME wasm store when it has a `Caller<'_, T>` /
// `StoreContextMut<'_, T>` in hand (wall #2 REFUTED). The ducklink
// callback path lowers to `&mut CoreStoreState` (store DATA, not store
// CONTEXT) because that is the `HasData<Data<'a> = &'a mut T>` shape
// wit-bindgen emits; and the outer `Manager::with_core` holds
// `Arc<Mutex<CoreExecution>>` non-reentrantly (wall #1 CONFIRMED). Both
// walls sit between a `duckdb:extension/nested-exec` callback and the
// primary core.
//
// This TLS is the low-touch bridge. `HostState::execute` snapshots raw
// pointers to the primary core's `Store<CoreStoreState>` + the
// [`duckdb_core_bindings::Libduckdb`] bindings + the CLI's active
// `ResourceAny` connection handle, sets them here for the duration of the
// outer `guest.call_execute`, and restores the previous slot on drop
// (RAII). A callback firing inside that outer call reads the TLS and
// re-enters the primary store directly through the raw store pointer —
// no re-lock of the outer mutex, and the write lands on the primary
// connection so the outer statement's continuation + the outer catalog
// see it.
//
// Safety invariant. The pointers are only dereferenced synchronously,
// from a host callback firing inside the outer `call_execute`, on the
// same thread that holds the outer `MutexGuard<CoreExecution>`. The RAII
// guard restores the previous slot on any exit path (Ok / Err / panic),
// so a leaked pointer to a freed `CoreExecution` is not reachable.
//
// The re-entry retains the shipped (b.1) sibling as a fallback for
// callers that reach `CoreServices::nested_exec` without the TLS set
// (narrow unit-test paths, or a future non-callback caller). See
// `docs/nested-exec-direction-1-plan.md` §7.7-7.8.

thread_local! {
    static PRIMARY_STORE_REENTRY: std::cell::Cell<Option<PrimaryReentry>> =
        const { std::cell::Cell::new(None) };
}

#[derive(Clone, Copy)]
struct PrimaryReentry {
    store: *mut Store<CoreStoreState>,
    bindings: *const duckdb_core_bindings::Libduckdb,
    connection: ResourceAny,
}

/// RAII: install `entry` in `PRIMARY_STORE_REENTRY` and restore the
/// previously-installed entry (typically `None`) on drop. Nesting-safe:
/// if a callback path ever recurses back into `HostState::execute`
/// (unlikely in the CLI shell, but the depth guard permits it up to
/// `NESTED_EXEC_MAX_DEPTH`), the inner guard overwrites the TLS for its
/// own frame and this guard's Drop restores the outer frame's entry.
struct PrimaryReentryGuard {
    prev: Option<PrimaryReentry>,
}

impl PrimaryReentryGuard {
    fn set(entry: PrimaryReentry) -> Self {
        let prev = PRIMARY_STORE_REENTRY.with(|slot| slot.replace(Some(entry)));
        Self { prev }
    }
}

impl Drop for PrimaryReentryGuard {
    fn drop(&mut self) {
        PRIMARY_STORE_REENTRY.with(|slot| slot.set(self.prev));
    }
}

impl CoreExecution {
    /// Phase 4 follow-up (FU4): wire this store to the shared post-LOAD
    /// registration archive. Callers pass `is_sibling = false` when this is
    /// the PRIMARY store (append-only writer) and `is_sibling = true` when
    /// this is a sibling core built by [`sibling_ensure_slot`] (read-only,
    /// serves the archive on the sibling's post-LOAD
    /// `get_pending_registrations`). Idempotent: replacing the archive
    /// mid-run would only happen in tests, and callers are expected to
    /// invoke this once right after `instantiate_core`.
    fn attach_replay_archive(
        &mut self,
        archive: Arc<Mutex<ducklink_runtime::PendingRegistrationsData>>,
        is_sibling: bool,
    ) {
        let data = self.store.data_mut();
        data.replay_archive = Some(archive);
        data.is_sibling = is_sibling;
    }

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


/// Compile the coarse `DUCKLINK_NETWORK_GRANT` environment variable into a
/// canonical [`datalink_policy::Policy`] for one extension:
///   - unset / empty / "none"  -> `Policy::deny_all()`  (default; secure)
///   - "all" / "*"             -> `Policy::allow_all()`  (grant every extension)
///   - otherwise               -> a comma/space-separated allowlist of names
///                                (e.g. "dns,http"): a listed extension gets
///                                `allow_all`, others `deny_all`.
///
/// This is the coarse-vs-fine ADAPTER: ducklink's single per-extension switch
/// is expressed against the same `Policy` type model sqlink uses per
/// capability/host-pattern, giving ducklink a path to per-extension policy
/// later with no behavior change today. Enforcement is unchanged — see
/// [`network_grant_allows`].
fn network_grant_policy(extension: &str) -> datalink_policy::Policy {
    network_grant_policy_for(std::env::var("DUCKLINK_NETWORK_GRANT").ok().as_deref(), extension)
}

/// Pure adapter: map a `DUCKLINK_NETWORK_GRANT` value (or `None` when unset)
/// for `extension` to a canonical [`Policy`](datalink_policy::Policy). Split
/// out from the env read so it is deterministically testable.
fn network_grant_policy_for(grant: Option<&str>, extension: &str) -> datalink_policy::Policy {
    use datalink_policy::Policy;
    match grant {
        Some(v) => {
            let v = v.trim();
            if v.is_empty() || v.eq_ignore_ascii_case("none") {
                return Policy::deny_all();
            }
            if v == "*" || v.eq_ignore_ascii_case("all") {
                return Policy::allow_all();
            }
            let listed = v
                .split([',', ' '])
                .map(str::trim)
                .any(|name| !name.is_empty() && name.eq_ignore_ascii_case(extension));
            if listed {
                Policy::allow_all()
            } else {
                Policy::deny_all()
            }
        }
        None => Policy::deny_all(),
    }
}

/// Best-effort network capability for an extension component: a thin check
/// against the canonical [`Policy`](datalink_policy::Policy) the
/// [`network_grant_policy`] adapter builds from `DUCKLINK_NETWORK_GRANT`.
/// True iff the policy grants the `Http` capability (network on).
///
/// Enforcement is the WasiCtx network grant: a denied extension's wasi:sockets
/// calls fail, so it cannot reach the network even though it may still try.
fn network_grant_allows(extension: &str) -> bool {
    network_grant_policy(extension).is_granted(datalink_policy::Capability::Http)
}

/// Store data for a dot-command component: just wasi (the component imports it
/// for std even though the `duckdb:dotcmd` world declares no WIT imports).
struct DotcmdState {
    wasi: WasiCtx,
    /// wasi:http host context (see the module-level `add_wasi_http_to_linker`
    /// import). Wired unconditionally so a future dot-command component can
    /// import wasi:http without a per-component gate.
    wasi_http: WasiHttpCtx,
    table: ResourceTable,
    /// The core (for spi SQL execution) and the CLI's live connection handle.
    core: Arc<Mutex<CoreExecution>>,
    current_connection: Arc<Mutex<Option<ResourceAny>>>,
    // Retained on the store data even though the surviving `spi.query` no
    // longer reads it — a follow-up dotcmd feature (e.g. the schema-qualified
    // prefix model migration) is expected to route through the manager again.
    #[allow(dead_code)]
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
impl WasiHttpView for DotcmdState {
    fn http(&mut self) -> WasiHttpCtxView<'_> {
        WasiHttpCtxView {
            ctx: &mut self.wasi_http,
            table: &mut self.table,
            hooks: Default::default(),
        }
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
        let mut core = self.core.lock().unwrap_or_else(|e| e.into_inner());
        let result = core
            .with_database(|guest, store| guest.call_execute(store, handle, &sql))
            .map_err(|trap| format!("spi query trapped: {trap}"))?;
        match result {
            Ok(qr) => Ok(spi_render_rows(qr)),
            Err(err) => Err(core_duckerror_message(err)),
        }
    }

    /// `duckdb:dotcmd/spi.edit` — shell out to the user's editor for a multi-
    /// line entry. Mirrors the standalone `fieldbook` CLI's editor UX exactly:
    /// EDITOR -> VISUAL -> `vi`; whitespace-split so `EDITOR="code -w"` works;
    /// temp file created in `std::env::temp_dir()`, unlinked after read.
    fn edit(&mut self, initial: String, hint_suffix: String) -> Result<String, String> {
        spi_edit(&initial, &hint_suffix)
    }
}

/// Launch `$EDITOR` (fallback: `$VISUAL`, then `vi`) on a temp file seeded with
/// `initial`. Returns the file contents after the editor exits. `hint_suffix`
/// (e.g. `.sql`) is appended to the temp filename for syntax-highlighting.
///
/// Free function (rather than a method on DotcmdState) so it has no state
/// dependency; tests + callers outside the Host trait can drive it directly.
fn spi_edit(initial: &str, hint_suffix: &str) -> Result<String, String> {
    use std::io::Write as _;

    // Same temp-file layout the standalone fieldbook binary uses:
    // `<tmp>/dotcmd-edit-<pid>-<epoch-ms><suffix>`. PID + timestamp keeps
    // concurrent invocations from colliding without needing a lockfile.
    let mut path = std::env::temp_dir();
    let millis = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0);
    let suffix = hint_suffix.trim();
    let filename = format!("dotcmd-edit-{}-{}{}", std::process::id(), millis, suffix);
    path.push(filename);

    // Seed the file with the initial contents (may be empty).
    {
        let mut f = std::fs::File::create(&path)
            .map_err(|e| format!("spi.edit: create tempfile {}: {e}", path.display()))?;
        if !initial.is_empty() {
            f.write_all(initial.as_bytes())
                .map_err(|e| format!("spi.edit: write tempfile {}: {e}", path.display()))?;
        }
    }

    // Resolve the editor. EDITOR wins; VISUAL fills in; vi is the ultimate
    // fallback (same order the standalone fieldbook CLI uses).
    let editor_spec = std::env::var("EDITOR")
        .or_else(|_| std::env::var("VISUAL"))
        .unwrap_or_else(|_| "vi".to_string());
    let mut parts = editor_spec.split_whitespace();
    let program = parts.next().unwrap_or("vi");
    let extra_args: Vec<&str> = parts.collect();

    // Blocking spawn — the editor owns the tty for the duration of the call.
    let status = std::process::Command::new(program)
        .args(&extra_args)
        .arg(&path)
        .status();
    let status = match status {
        Ok(s) => s,
        Err(e) => {
            let _ = std::fs::remove_file(&path);
            return Err(format!(
                "spi.edit: could not spawn editor {editor_spec:?}: {e}"
            ));
        }
    };
    if !status.success() {
        let _ = std::fs::remove_file(&path);
        return Err(format!(
            "spi.edit: editor {editor_spec:?} exited with {status}"
        ));
    }
    let contents = std::fs::read_to_string(&path)
        .map_err(|e| format!("spi.edit: read tempfile {}: {e}", path.display()));
    let _ = std::fs::remove_file(&path);
    contents
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
        // @5.0.0: first-class 128-bit integer values.
        core_types::Duckvalue::Hugeint(h) => format_hugeint(h.lower, h.upper),
        core_types::Duckvalue::Uhugeint(h) => format_uhugeint(h.lower, h.upper),
        // ESCAPE-HATCH: the value is already JSON; emit it verbatim.
        core_types::Duckvalue::Complex(c) => c.json.clone(),
    }
}

/// Render a signed 128-bit integer split into (lower: u64, upper: i64) halves.
pub(crate) fn format_hugeint(lower: u64, upper: i64) -> String {
    let raw = (((upper as u64 as u128) << 64) | lower as u128) as i128;
    raw.to_string()
}
/// Render an unsigned 128-bit integer split into (lower, upper) 64-bit halves.
pub(crate) fn format_uhugeint(lower: u64, upper: u64) -> String {
    let raw = ((upper as u128) << 64) | lower as u128;
    raw.to_string()
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
        add_wasi_http_to_linker(&mut linker)?;
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
            Some(compose_dynlink::new_resident(
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
                wasi_http: WasiHttpCtx::new(),
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
    callback_registry: Arc<RwLock<CallbackRegistry>>,
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
    // 2.3.0 / v3: parser extensions components have declared via
    // `parser.register-parser-extension`. The core pulls this list (through
    // `parser-host.parser-list`) and, when its built-in parser rejects a statement,
    // offers the text via `parser-host.call-parse`; the owning component returns a
    // string->SQL rewrite. Keyed by parser name -> (extension, callback-handle).
    parsers: HashMap<String, (String, u32)>,
    // 2.3.0 / v3: optimizer rules components have declared via
    // `optimizer.register-optimizer-rule`. The core registers a component-driven
    // OptimizerExtension (via optimizer-host.optimizer-list) and offers the
    // flattened plan via `optimizer-host.call-optimize`. Keyed by rule name ->
    // (extension, callback-handle).
    optimizers: HashMap<String, (String, u32)>,
    // Phase 2c (@5): host-owned table-fn callback registry backing the ATTACH
    // intercept's read-path materialisation. Keyed by callback handle
    // (allocated inside the [`AT5_ATTACH_SCAN_HANDLE_MIN`]..[`AT5_ATTACH_SCAN_HANDLE_MAX`]
    // range so `dispatch_table` distinguishes them from ordinary extension
    // handles), value is the `(extension, catalog_handle, table)` triple the
    // handler needs to route into `ExtensionInstance::storage_scan_*`.
    at5_scan_targets: HashMap<u32, At5ScanTarget>,
    /// Next AT5 attach-scan handle to hand out. Grows monotonically inside
    /// the reserved sentinel range; wraps back to [`AT5_ATTACH_SCAN_HANDLE_MIN`]
    /// on overflow (~4M in-flight ATTACHed tables before overlap — well beyond
    /// any realistic workload).
    next_at5_scan_handle: u32,
    // 3.1.0 additive minor: streaming + filter-pushdown table functions components
    // have declared via `table-stream.register-filterable-table`. The core pulls
    // this list (through `table-stream-host.filterable-table-list`), registers a
    // real C++ streaming TableFunction (filter_pushdown = true) for each, and at
    // scan time drives the owning component's `call-table-open-filtered` via
    // `table-stream-host` (ts-open-filtered/next/close). Keyed by the global
    // routable callback handle so dispatch routes back to the owning component.
    filterable_tables: Vec<reg::FilterableTableReg>,
    // v1.1 live-query host import: the CLI's live connection, shared so a
    // query-capable component's `query` import (catalog completion) runs on the
    // same connection the user is on. Cloned into each component's CoreServices.
    current_connection: Arc<Mutex<Option<ResourceAny>>>,
    // v1.1 live-query host import: the re-entrancy fallback catalog snapshot,
    // shared with each component's CoreServices + refreshed at CLI boundaries.
    catalog_snapshot: Arc<Mutex<CatalogSnapshot>>,
    // Multi-provider resolver (design A, PLAN-multi-provider-extensions.md): the
    // parsed registry manifest, the resolution policy (`SET extension_provider` /
    // deny), and the last per-extension resolution reasoning (for the
    // `extension_provider(...)` observability function). LOAD resolves a provider
    // through the resolver instead of the bare filename shortcut.
    registry_index: Arc<serde_json::Value>,
    resolver_policy: resolver::ResolvePolicy,
    last_resolutions: HashMap<String, String>,
    // Phase D: per-sub-extension `compose:dynlink` bridge + composed-provider
    // loader. Holds the `sub_ext -> {plan, bridge, derived-from}` maps and the
    // materialize-on-first-LOAD composer. `ensure_extension_loaded` consults
    // `sub_ext_loader.has_bridge(name)` BEFORE the flat
    // `<extensions-dir>/<name>.wasm` shortcut and, when it matches, composes
    // + registers the sub-ext's provider (once) and loads the bridge wasm
    // instead. Registered against the process-global `ProviderRegistry` so
    // one composed provider serves every bridge that plugs it (the
    // postgis + mobilitydb dedup path).
    sub_ext_loader: sub_ext::SubExtLoader,
    // One-shot guard for the synthetic `ducklink_load` table-fn registration
    // (surfaced through the [`DUCKLINK_LOAD_HANDLE`] sentinel). The first
    // `drain_pending_registrations` after startup appends a hand-built
    // [`reg::TableReg`] to the drain output so the core catalog acquires
    // `ducklink_load` alongside whatever the freshly loaded extension
    // registered. Subsequent drains skip re-injection — the catalog entry is
    // process-lived.
    injected_ducklink_load: bool,
    // Same one-shot guard as [`injected_ducklink_load`], but for the
    // synthetic `ducklink_prefix(alias, namespace)` TF + scalar surfaced
    // through the [`DUCKLINK_PREFIX_TABLE_HANDLE`] +
    // [`DUCKLINK_PREFIX_SCALAR_HANDLE`] sentinels. Two entries flip on
    // the same bool because they're always injected together.
    injected_ducklink_prefix: bool,
    // `(alias, namespace)` pairs the native `ducklink_prefix` handler
    // validated and queued for deferred DDL. The handler runs INSIDE a
    // `dispatch_scalar`/`dispatch_table` callback, i.e. the core wasm
    // store is mid-call — so the actual `duckdb_functions()` scan +
    // per-function `CREATE OR REPLACE MACRO` + `INSERT INTO
    // ducklink.prefixes` cannot happen in-band. Drained by
    // [`HostState::flush_deferred_prefix_declarations`] on the next
    // execute boundary (when the core is idle again).
    deferred_prefix_declarations: Vec<(String, String)>,
    // Names loaded by `ducklink_load(name)` awaiting a core-side drain.
    //
    // The native `ducklink_load` handler runs *inside* a `call_table`, i.e.
    // the wasm core store is mid-call — we cannot re-enter `call_execute` to
    // issue `LOAD <name>;` and trigger the standard post-LOAD drain (the same
    // re-entrancy shape the live-query import respects in `query`). So the
    // handler stashes what it loaded here and [`HostState::execute`] issues
    // an idempotent `LOAD <name>;` on the user's next statement — the second
    // LOAD lands `ensure_extension_loaded` on its fast path (already in
    // `self.extensions`) and the core then drains `deferred_registrations`
    // into its catalog.
    deferred_drain_names: Vec<String>,
    // Registrations captured out of `ExtensionInstance`s loaded via
    // `ducklink_load(name)` but not yet handed to the core (see
    // `deferred_drain_names`).
    //
    // The native handler drains the just-loaded instance so it can count
    // scalars/tables/aggregates for its return row; that drain empties the
    // instance's own queue, so the next `drain_pending_registrations` won't
    // find them there. Instead we prepend `deferred_registrations` to the
    // core's next drain output so the registrations still reach the DuckDB
    // catalog on the deferred `LOAD`.
    deferred_registrations: PendingRegistrationsData,
    // The nested-exec (Direction-1 §5.(b.1)) sibling-core state. `Some` when
    // the host wired one at construction (the CLI / harness paths do this);
    // `None` for narrow test paths that never opt in. Cloned into every
    // component's `CoreServices` at load so first `nested_exec` lazy-inits the
    // shared sibling core.
    sibling: Option<Arc<SiblingState>>,
    // Phase 3 (@5 host-import wiring): registry of secret TYPE / PROVIDER
    // backings declared by extensions via `secret.register-secret-type` /
    // `register-secret-provider`. Keyed by `(type_name, provider)` — `provider`
    // is `None` for a bare type registration (the type's default/config
    // provider). The value is `(extension_name, callback_handle)` — the same
    // shape as `storage_backends`. A future `CREATE SECRET (TYPE ..., PROVIDER
    // ..., k v)` interceptor will consult this map and dispatch to the backing
    // component's `secret-dispatch::create-secret` export via
    // `ExtensionInstance::create_secret`.
    secret_backends: HashMap<(String, Option<String>), (String, u32)>,
    // Phase 3 (@5 host-import wiring): registry of configuration options
    // declared by extensions via `settings.register-option`. Keyed by option
    // name; the value carries the full [`reg::SettingReg`] (extension name,
    // description, type, default, scope). A future `SET <name> = <value>`
    // interceptor will consult this map and forward the change to the backing
    // component's `settings-dispatch::on-setting-set` export via
    // `ExtensionInstance::setting_set` (once handle allocation for
    // register-option lands — see the field doc on
    // `reg::SettingReg`).
    settings_registry: HashMap<String, (String, reg::SettingReg)>,
}

impl ExtensionManager {
    fn new(engine: Engine) -> Self {
        let index_path = workspace_root().join("registry/index.json");
        let registry_index = Arc::new(
            std::fs::read_to_string(&index_path)
                .ok()
                .and_then(|t| serde_json::from_str::<serde_json::Value>(&t).ok())
                .unwrap_or(serde_json::Value::Null),
        );
        // Seed the resolution policy from the environment (the `SET
        // extension_provider` / deny analog at startup; the runtime
        // `set_extension_provider(...)` function updates it live).
        let forced_provider = std::env::var("DUCKLINK_EXTENSION_PROVIDER")
            .ok()
            .filter(|s| !s.is_empty());
        let denied = std::env::var("DUCKLINK_EXTENSION_PROVIDER_DENY")
            .ok()
            .map(|s| {
                s.split(',')
                    .map(|x| x.trim().to_string())
                    .filter(|x| !x.is_empty())
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();
        // Phase D: the sub-ext loader shares the process-global
        // `ProviderRegistry` (built lazily against `engine`) so a composed
        // provider registered here on first LOAD resolves through the same
        // registry as the `DUCKLINK_PROVIDERS`-declared ones and every
        // bridge component sees it.
        let sub_ext_loader =
            sub_ext::SubExtLoader::from_env(dynlink_provider_registry(&engine).clone());
        Self {
            engine,
            core: None,
            extensions: HashMap::new(),
            callback_registry: Arc::new(RwLock::new(CallbackRegistry::new())),
            storage_backends: HashMap::new(),
            index_backends: HashMap::new(),
            files_backend: None,
            collations: HashMap::new(),
            pragmas: HashMap::new(),
            parsers: HashMap::new(),
            optimizers: HashMap::new(),
            filterable_tables: Vec::new(),
            current_connection: Arc::new(Mutex::new(None)),
            catalog_snapshot: Arc::new(Mutex::new(CatalogSnapshot::default())),
            registry_index,
            resolver_policy: resolver::ResolvePolicy {
                forced_provider,
                denied,
            },
            last_resolutions: HashMap::new(),
            sub_ext_loader,
            injected_ducklink_load: false,
            injected_ducklink_prefix: false,
            deferred_prefix_declarations: Vec::new(),
            deferred_drain_names: Vec::new(),
            deferred_registrations: PendingRegistrationsData::default(),
            sibling: None,
            at5_scan_targets: HashMap::new(),
            next_at5_scan_handle: AT5_ATTACH_SCAN_HANDLE_MIN,
            secret_backends: HashMap::new(),
            settings_registry: HashMap::new(),
        }
    }

    /// The canonical conformance `suite_digest` the resolver HOLDS for this
    /// extension at its live contract — the content digest of the on-disk
    /// conformance suite (`<source>/conformance.sql` + `conformance.expected`),
    /// computed via the shared scheme in `resolver::compute_suite_digest`. The
    /// suite file is the source of truth, so a tampered/stale provider record
    /// (whose `suite_digest` no longer matches the file) is caught by the gate.
    ///
    /// Returns `None` when the extension has no conformance suite yet (the long
    /// tail not promoted): the resolver then falls back to reference-by-construction
    /// for that extension (see `resolver::conformance_ok`).
    fn canonical_suite_digest(&self, name: &str) -> Option<String> {
        let exts = self.registry_index.get("extensions")?.as_array()?;
        let entry = exts
            .iter()
            .find(|e| e.get("name").and_then(|v| v.as_str()) == Some(name))?;
        let source = entry.get("source").and_then(|v| v.as_str())?;
        let dir = workspace_root().join(source);
        let sql = std::fs::read_to_string(dir.join("conformance.sql")).ok()?;
        let expected = std::fs::read_to_string(dir.join("conformance.expected")).ok()?;
        Some(resolver::compute_suite_digest(&sql, &expected))
    }

    /// Multi-provider resolution for a logical extension: read its manifest entry
    /// (providers[] or backward-compat single-artifact) and run the resolver
    /// candidate pipeline (conformance gate -> available -> trusted -> !excluded
    /// -> precedence). Returns the chosen provider's on-disk artifact path
    /// (located within the configured extensions dir, honoring --extensions-dir),
    /// recording the per-candidate reasoning for observability. Extensions absent
    /// from the manifest fall back to the bare filename (backward-compat).
    fn resolve_provider_artifact(&mut self, name: &str) -> Result<PathBuf, String> {
        let entry = match resolver::read_manifest_entry(&self.registry_index, name) {
            Some(e) => e,
            None => {
                // No manifest entry: backward-compat filename resolution.
                self.last_resolutions.insert(
                    name.to_string(),
                    "no manifest entry; backward-compat filename resolution".to_string(),
                );
                return Ok(extension_artifact_path(name));
            }
        };
        let env = resolver::Env {
            available_components: resolver::available_components_from_env(),
            ..resolver::Env::default()
        };
        let canonical = self.canonical_suite_digest(name);
        match resolver::resolve(&entry, &env, &self.resolver_policy, canonical.as_deref()) {
            Ok(res) => {
                let reasoning = resolver::render_reasoning(&res.reasoning);
                eprintln!(
                    "[resolver] '{name}' -> provider '{}' [{}] (contract {}); {}",
                    res.chosen_id,
                    res.chosen_kind,
                    short_digest(&entry.wit_contract),
                    reasoning
                );
                self.last_resolutions.insert(
                    name.to_string(),
                    format!(
                        "chosen: {} [{}] at contract {}; {}",
                        res.chosen_id,
                        res.chosen_kind,
                        short_digest(&entry.wit_contract),
                        reasoning
                    ),
                );
                // Locate the chosen provider's artifact within the extensions dir
                // (its basename), so --extensions-dir stays the source of truth.
                let root = EXTENSION_ROOT
                    .get()
                    .cloned()
                    .unwrap_or_else(|| workspace_root().join("artifacts/extensions"));
                let basename = match &res.artifact {
                    resolver::ContentRef::Path(p) => p
                        .file_name()
                        .map(|f| f.to_owned())
                        .unwrap_or_else(|| std::ffi::OsString::from(format!("{name}.wasm"))),
                    _ => std::ffi::OsString::from(format!("{name}.wasm")),
                };
                Ok(root.join(basename))
            }
            Err(e) => {
                let reasoning = resolver::render_reasoning(&e.reasoning);
                self.last_resolutions
                    .insert(name.to_string(), format!("FAILED: {reasoning}"));
                Err(e.to_string())
            }
        }
    }

    /// Observability for `extension_provider('<ext>')`: run the resolver as a
    /// dry-run and render the chosen provider + why each loser lost.
    fn explain_resolution(&self, name: &str) -> String {
        match resolver::read_manifest_entry(&self.registry_index, name) {
            None => format!("'{name}': no manifest entry (backward-compat filename load)"),
            Some(entry) => {
                let env = resolver::Env {
                    available_components: resolver::available_components_from_env(),
                    ..resolver::Env::default()
                };
                let canonical = self.canonical_suite_digest(name);
                match resolver::resolve(&entry, &env, &self.resolver_policy, canonical.as_deref()) {
                    Ok(res) => format!(
                        "'{name}': chosen '{}' [{}] at contract {}; {}",
                        res.chosen_id,
                        res.chosen_kind,
                        short_digest(&entry.wit_contract),
                        resolver::render_reasoning(&res.reasoning)
                    ),
                    Err(e) => format!("'{name}': {e}"),
                }
            }
        }
    }

    /// `set_extension_provider('<id>')`: force a provider id for subsequent LOADs.
    fn set_forced_provider(&mut self, id: &str) -> String {
        if id.is_empty() || id.eq_ignore_ascii_case("auto") || id.eq_ignore_ascii_case("none") {
            self.resolver_policy.forced_provider = None;
            "extension_provider override cleared (auto)".to_string()
        } else {
            self.resolver_policy.forced_provider = Some(id.to_string());
            format!("extension_provider forced to '{id}'")
        }
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

    /// 2.3.0 / v3: the parser extensions components have declared, as
    /// (name, callback-handle). The core pulls this through `parser-host.parser-list`
    /// and offers rejected statements to each via `parser-host.call-parse`.
    fn registered_parsers(&self) -> Vec<(String, u32)> {
        self.parsers
            .iter()
            .map(|(name, (_extension, handle))| (name.clone(), *handle))
            .collect()
    }

    /// 2.3.0 / v3: offer a parser-rejected statement to the parser extension that
    /// owns `handle`, returning `Some(rewrite_sql)` if it claims it, else `None`.
    /// Drives the owning component's `parser-dispatch.call-parse`.
    fn dispatch_parse(
        &mut self,
        handle: u32,
        query: &str,
    ) -> Result<Option<String>, extension_types::Duckerror> {
        let ext = self
            .parsers
            .values()
            .find(|(_e, h)| *h == handle)
            .map(|(e, _h)| e.clone())
            .ok_or_else(|| {
                extension_types::Duckerror::Invalidstate(format!(
                    "no parser extension registered for handle {handle}"
                ))
            })?;
        let instance = self.extensions.get_mut(&ext).ok_or_else(|| {
            extension_types::Duckerror::Invalidstate(format!("parser extension '{ext}' not loaded"))
        })?;
        let outcome = instance.call_parse(handle, query)?;
        // Defensive boundary: the core RE-PLANS the returned rewrite, so reject an
        // adversarial/degenerate rewrite here (see `validate_parser_rewrite`).
        if let Some(rewrite) = &outcome {
            validate_parser_rewrite(&ext, query, rewrite)
                .map_err(extension_types::Duckerror::Invalidargument)?;
        }
        Ok(outcome)
    }

    /// 2.3.0 / v3: the optimizer rules components have declared, as
    /// (rule-name, callback-handle). The core registers the component-driven
    /// OptimizerExtension when this is non-empty.
    fn registered_optimizers(&self) -> Vec<(String, u32)> {
        self.optimizers
            .iter()
            .map(|(name, (_extension, handle))| (name.clone(), *handle))
            .collect()
    }

    /// 2.3.0 / v3: offer the flattened plan (`plan_json` from the core) to the
    /// optimizer rule that owns `handle`. Parses the neutral JSON node list, drives
    /// the owning component's `optimizer-dispatch.call-optimize`, and returns the
    /// `rewrite-query` SQL (or None for declined / structured-apply).
    fn dispatch_optimize(
        &mut self,
        handle: u32,
        plan_json: &str,
    ) -> Result<Option<String>, extension_types::Duckerror> {
        let ext = self
            .optimizers
            .values()
            .find(|(_e, h)| *h == handle)
            .map(|(e, _h)| e.clone())
            .ok_or_else(|| {
                extension_types::Duckerror::Invalidstate(format!(
                    "no optimizer rule registered for handle {handle}"
                ))
            })?;
        // Parse the core's flattened plan JSON: [{"id":N,"op":"X","parent":P,"table":"T"?}].
        // Flattening lives in the wit-free, fuzzed `plan_shape` module (never-panic
        // boundary; bounds the node count against an adversarial core).
        let nodes = crate::plan_shape::flatten_plan_json(plan_json)
            .map_err(extension_types::Duckerror::Invalidargument)?;
        let instance = self.extensions.get_mut(&ext).ok_or_else(|| {
            extension_types::Duckerror::Invalidstate(format!("optimizer extension '{ext}' not loaded"))
        })?;
        instance.call_optimize(handle, nodes, "")
    }

    /// The ATTACH `TYPE` names of every storage backend a component has
    /// registered (via `register-storage`). The core pulls this list (through the
    /// `storage-host.storage-list-types` import) and registers a wasm
    /// StorageExtension for each, so `ATTACH ... (TYPE <name>)` dispatches here.
    #[allow(dead_code)]
    fn registered_storage_types(&self) -> Vec<String> {
        self.storage_backends.keys().cloned().collect()
    }

    /// Phase 2 (@5) B1: lookup the storage backend for a specific TYPE name
    /// parsed from the intercepted ATTACH SQL. Returns
    /// `(extension-id, callback-handle)` or `None` if no extension has
    /// registered a backend under `type_name`.
    pub fn storage_backend_for(&self, type_name: &str) -> Option<(String, u32)> {
        self.storage_backends
            .get(type_name)
            .map(|(ext, handle)| (ext.clone(), *handle))
    }

    /// Phase 3 (@5) host-import lookup: the backing extension + callback
    /// handle for a `(type_name, provider)` secret registration, or `None`
    /// when no extension declared one. `provider = None` looks up the type's
    /// default/config provider; `Some(p)` looks up the named provider first
    /// and falls back to the default when no named-provider entry exists (a
    /// component may declare only the type without any named provider). The
    /// future `CREATE SECRET` interceptor uses this to route to the backing
    /// component's `secret-dispatch::create-secret` export via
    /// [`ExtensionInstance::create_secret`].
    #[allow(dead_code)]
    pub fn secret_backend_for(
        &self,
        type_name: &str,
        provider: Option<&str>,
    ) -> Option<(String, u32)> {
        if let Some(p) = provider {
            if let Some((ext, handle)) =
                self.secret_backends.get(&(type_name.to_string(), Some(p.to_string())))
            {
                return Some((ext.clone(), *handle));
            }
        }
        self.secret_backends
            .get(&(type_name.to_string(), None))
            .map(|(ext, handle)| (ext.clone(), *handle))
    }

    /// Phase 3 (@5) host-import lookup: the backing extension + full
    /// [`reg::SettingReg`] for an option name previously declared via
    /// `settings.register-option`, or `None` when no extension declared it.
    /// The future `SET <name> = <value>` interceptor uses this to route the
    /// change to the declaring component's `settings-dispatch::on-setting-set`
    /// export via [`ExtensionInstance::setting_set`].
    #[allow(dead_code)]
    pub fn setting_backend_for(&self, name: &str) -> Option<(String, reg::SettingReg)> {
        self.settings_registry
            .get(name)
            .map(|(ext, reg)| (ext.clone(), reg.clone()))
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

    pub fn dispatch_storage_list_tables(
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

    /// AT5 write-back: serialize the extension's in-memory representation of
    /// `catalog` back to a byte blob. The host calls this after every
    /// successful INSERT/UPDATE/DELETE dispatch and writes the returned bytes
    /// back to the ATTACH DSN file so the mutation persists on disk. Backends
    /// whose storage isn't a serializable blob return
    /// `Duckerror::Unsupported`, which the caller treats as a silent no-op.
    pub fn dispatch_storage_serialize(
        &mut self,
        catalog: u32,
    ) -> Result<Vec<u8>, extension_types::Duckerror> {
        let (ext, handle) = self.resolve_storage_backend()?;
        let instance = self.extensions.get_mut(&ext).ok_or_else(|| {
            extension_types::Duckerror::Invalidstate(format!("storage extension '{ext}' not loaded"))
        })?;
        instance.storage_serialize(handle, catalog)
    }

    /// Bug 4b: probe the extension's `writes_persist_directly` capability.
    /// Returns Ok(true) for a backend that is authoritative for durability
    /// on its own — e.g. sqlitewasm which self-persists via wasivfs into the
    /// host's preopens, or mysql/postgres which write over the wire. The
    /// caller (intercept_attach) caches the flag on AttachedForeignCatalog;
    /// at5_write_back short-circuits when it's true. Ok(false) or an
    /// Unsupported/Internal error means "not self-persisting" — the legacy
    /// serialize + host-side fs write-back path applies (silent no-op for
    /// backends whose serialize also returns Unsupported).
    pub fn dispatch_storage_writes_persist_directly(
        &mut self,
    ) -> Result<bool, extension_types::Duckerror> {
        let (ext, handle) = self.resolve_storage_backend()?;
        let instance = self.extensions.get_mut(&ext).ok_or_else(|| {
            extension_types::Duckerror::Invalidstate(format!("storage extension '{ext}' not loaded"))
        })?;
        instance.storage_writes_persist_directly(handle)
    }

    // --- Phase 2c (@5): AT5 attach-scan callback registry ---

    /// Allocate a fresh AT5 attach-scan callback handle (inside the reserved
    /// [`AT5_ATTACH_SCAN_HANDLE_MIN`]..[`AT5_ATTACH_SCAN_HANDLE_MAX`] range) and
    /// remember what `(extension, catalog, table)` the core should route to
    /// when it fires `call-table(handle, _)`. Returns the handle to give to
    /// `database.register-table-function`.
    fn register_at5_scan(
        &mut self,
        extension: String,
        storage_handle: u32,
        catalog_handle: u32,
        table: String,
    ) -> u32 {
        let mut handle = self.next_at5_scan_handle;
        // Advance; wrap back to MIN once we cross MAX. Scan the map to skip
        // any still-live handle after a wrap so we never collide (~4M range
        // makes wrap-around a theoretical concern only).
        let mut next = handle.wrapping_add(1);
        if next > AT5_ATTACH_SCAN_HANDLE_MAX || next < AT5_ATTACH_SCAN_HANDLE_MIN {
            next = AT5_ATTACH_SCAN_HANDLE_MIN;
        }
        while self.at5_scan_targets.contains_key(&handle) {
            handle = next;
            next = next.wrapping_add(1);
            if next > AT5_ATTACH_SCAN_HANDLE_MAX || next < AT5_ATTACH_SCAN_HANDLE_MIN {
                next = AT5_ATTACH_SCAN_HANDLE_MIN;
            }
        }
        self.next_at5_scan_handle = next;
        self.at5_scan_targets.insert(
            handle,
            At5ScanTarget {
                extension,
                storage_handle,
                catalog_handle,
                table,
            },
        );
        handle
    }

    /// Dispatch handler for handles allocated by `register_at5_scan`. The
    /// core has just fired `call-table(handle, _args)` for a
    /// `__<alias>_<table>()` name; we drain the extension's
    /// storage-dispatch scan and return the full resultset. Row-by-row
    /// streaming is a Phase-2d follow-up (see ADR Amendment A5 §3).
    fn native_at5_attach_scan(
        &mut self,
        handle: u32,
        _args: &[extension_types::Duckvalue],
    ) -> Result<extension_runtime::Resultset, extension_types::Duckerror> {
        let target = self.at5_scan_targets.get(&handle).cloned().ok_or_else(|| {
            extension_types::Duckerror::Invalidstate(format!(
                "unknown AT5 attach-scan handle {handle}"
            ))
        })?;
        // Build a "scan everything" request: empty projection = all columns,
        // no filters, no limit. Filter/projection pushdown from the wasm core
        // through `register-table-function` is a Phase-2d follow-up.
        let request = storage_scan::ScanRequest {
            table: target.table.clone(),
            projection: Vec::new(),
            filters: Vec::new(),
            limit: None,
        };
        let instance = self.extensions.get_mut(&target.extension).ok_or_else(|| {
            extension_types::Duckerror::Invalidstate(format!(
                "AT5 attach-scan handle {handle} points at extension '{}', which is not loaded",
                target.extension
            ))
        })?;
        let scan = instance.storage_scan_open(target.storage_handle, target.catalog_handle, request)?;
        // Drain in one batch. The MAX u32 acts as "give me everything you have";
        // extensions that honour it (sqlitewasm does) return the whole cursor.
        let mut out: Vec<Vec<extension_types::Duckvalue>> = Vec::new();
        loop {
            let batch = instance.storage_scan_next(target.storage_handle, scan, u32::MAX)?;
            if batch.is_empty() {
                break;
            }
            let batch_len = batch.len();
            out.extend(batch);
            // Belt-and-braces: guard against a mis-implementing extension
            // that keeps returning the same tail forever. u32::MAX bound
            // means the second call should observe EOF.
            if batch_len == 0 {
                break;
            }
        }
        let _ = instance.storage_scan_close(target.storage_handle, scan);
        Ok(out)
    }

    // --- M2c write surface: transactions + DDL + DML ---
    //
    // Each method resolves the (ext, handle) pair the storage backend
    // registered (same picker as the read-side scan) and forwards to the
    // ExtensionInstance's `storage_*` write trampoline
    // (ducklink-runtime/src/extension.rs). Errors bubble up as
    // `extension_types::Duckerror` which the core-side impl re-maps.

    fn dispatch_storage_begin_transaction(
        &mut self,
        catalog: u32,
    ) -> Result<u32, extension_types::Duckerror> {
        let (ext, handle) = self.resolve_storage_backend()?;
        let instance = self.extensions.get_mut(&ext).ok_or_else(|| {
            extension_types::Duckerror::Invalidstate(format!("storage extension '{ext}' not loaded"))
        })?;
        instance.storage_begin_transaction(handle, catalog)
    }

    fn dispatch_storage_commit_transaction(
        &mut self,
        txn: u32,
    ) -> Result<(), extension_types::Duckerror> {
        let (ext, handle) = self.resolve_storage_backend()?;
        let instance = self.extensions.get_mut(&ext).ok_or_else(|| {
            extension_types::Duckerror::Invalidstate(format!("storage extension '{ext}' not loaded"))
        })?;
        instance.storage_commit_transaction(handle, txn)
    }

    fn dispatch_storage_rollback_transaction(
        &mut self,
        txn: u32,
    ) -> Result<(), extension_types::Duckerror> {
        let (ext, handle) = self.resolve_storage_backend()?;
        let instance = self.extensions.get_mut(&ext).ok_or_else(|| {
            extension_types::Duckerror::Invalidstate(format!("storage extension '{ext}' not loaded"))
        })?;
        instance.storage_rollback_transaction(handle, txn)
    }

    fn dispatch_storage_create_table(
        &mut self,
        txn: u32,
        table: &str,
        columns: &[extension_types::Columndef],
    ) -> Result<(), extension_types::Duckerror> {
        let (ext, handle) = self.resolve_storage_backend()?;
        let instance = self.extensions.get_mut(&ext).ok_or_else(|| {
            extension_types::Duckerror::Invalidstate(format!("storage extension '{ext}' not loaded"))
        })?;
        instance.storage_create_table(handle, txn, table, columns)
    }

    fn dispatch_storage_insert_rows(
        &mut self,
        txn: u32,
        table: &str,
        rows: &[Vec<extension_types::Duckvalue>],
    ) -> Result<u64, extension_types::Duckerror> {
        let (ext, handle) = self.resolve_storage_backend()?;
        let instance = self.extensions.get_mut(&ext).ok_or_else(|| {
            extension_types::Duckerror::Invalidstate(format!("storage extension '{ext}' not loaded"))
        })?;
        instance.storage_insert_rows(handle, txn, table, rows)
    }

    /// Phase 2 (@5) intercept helper: wrap a single INSERT dispatch in an
    /// auto-BEGIN/COMMIT so the ATTACH-intercept write path doesn't need to
    /// manage transaction lifecycles itself. Rolls back on failure. Used
    /// exclusively by `HostState::intercept_write`.
    pub fn dispatch_storage_insert_direct(
        &mut self,
        catalog: u32,
        table: &str,
        rows: &[Vec<extension_types::Duckvalue>],
    ) -> Result<u64, extension_types::Duckerror> {
        let txn = self.dispatch_storage_begin_transaction(catalog)?;
        match self.dispatch_storage_insert_rows(txn, table, rows) {
            Ok(n) => {
                self.dispatch_storage_commit_transaction(txn)?;
                Ok(n)
            }
            Err(err) => {
                // Roll back on error; ignore any secondary rollback failure --
                // the original error is what matters to the caller.
                let _ = self.dispatch_storage_rollback_transaction(txn);
                Err(err)
            }
        }
    }

    fn dispatch_storage_delete_rows(
        &mut self,
        txn: u32,
        table: &str,
        rowids: &[i64],
    ) -> Result<u64, extension_types::Duckerror> {
        let (ext, handle) = self.resolve_storage_backend()?;
        let instance = self.extensions.get_mut(&ext).ok_or_else(|| {
            extension_types::Duckerror::Invalidstate(format!("storage extension '{ext}' not loaded"))
        })?;
        instance.storage_delete_rows(handle, txn, table, rowids)
    }

    fn dispatch_storage_update_rows(
        &mut self,
        txn: u32,
        table: &str,
        rowids: &[i64],
        updated_columns: &[u32],
        rows: &[Vec<extension_types::Duckvalue>],
    ) -> Result<u64, extension_types::Duckerror> {
        let (ext, handle) = self.resolve_storage_backend()?;
        let instance = self.extensions.get_mut(&ext).ok_or_else(|| {
            extension_types::Duckerror::Invalidstate(format!("storage extension '{ext}' not loaded"))
        })?;
        // @5.0.0 dropped the `updated_columns` mask from storage-write-dispatch
        // (the row is now taken as a whole write).
        let _ = updated_columns;
        instance.storage_update_rows(handle, txn, table, rowids, rows)
    }

    /// Amendment A5 counterpart to `dispatch_storage_insert_direct`. Wraps a
    /// single `update-rows` dispatch in an auto-BEGIN/COMMIT so the write
    /// intercept doesn't manage transaction lifecycles itself. Rolls back on
    /// failure. Used exclusively by `HostState::intercept_write`.
    pub fn dispatch_storage_update_direct(
        &mut self,
        catalog: u32,
        table: &str,
        rowids: &[i64],
        rows: &[Vec<extension_types::Duckvalue>],
    ) -> Result<u64, extension_types::Duckerror> {
        let txn = self.dispatch_storage_begin_transaction(catalog)?;
        match self.dispatch_storage_update_rows(txn, table, rowids, &[], rows) {
            Ok(n) => {
                self.dispatch_storage_commit_transaction(txn)?;
                Ok(n)
            }
            Err(err) => {
                let _ = self.dispatch_storage_rollback_transaction(txn);
                Err(err)
            }
        }
    }

    /// Auto-BEGIN/COMMIT wrapper around `storage-write-dispatch.create-table`
    /// for the write-intercept path. Used exclusively by
    /// `HostState::intercept_write`.
    pub fn dispatch_storage_create_table_direct(
        &mut self,
        catalog: u32,
        table: &str,
        columns: &[extension_types::Columndef],
    ) -> Result<(), extension_types::Duckerror> {
        let txn = self.dispatch_storage_begin_transaction(catalog)?;
        match self.dispatch_storage_create_table(txn, table, columns) {
            Ok(()) => {
                self.dispatch_storage_commit_transaction(txn)?;
                Ok(())
            }
            Err(err) => {
                let _ = self.dispatch_storage_rollback_transaction(txn);
                Err(err)
            }
        }
    }

    /// Amendment A5 counterpart to `dispatch_storage_insert_direct`. Wraps a
    /// single `delete-rows` dispatch in an auto-BEGIN/COMMIT. Rolls back on
    /// failure.
    pub fn dispatch_storage_delete_direct(
        &mut self,
        catalog: u32,
        table: &str,
        rowids: &[i64],
    ) -> Result<u64, extension_types::Duckerror> {
        let txn = self.dispatch_storage_begin_transaction(catalog)?;
        match self.dispatch_storage_delete_rows(txn, table, rowids) {
            Ok(n) => {
                self.dispatch_storage_commit_transaction(txn)?;
                Ok(n)
            }
            Err(err) => {
                let _ = self.dispatch_storage_rollback_transaction(txn);
                Err(err)
            }
        }
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

    /// nested-exec Direction-1 §5.(b.1): attach a shared [`SiblingState`] so
    /// each component's [`CoreServices`] can service `nested_exec` from the
    /// lazily-materialized second core.
    fn attach_sibling_state(&mut self, sibling: Arc<SiblingState>) {
        self.sibling = Some(sibling);
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
        // `ducklink_prefix` sentinel (scalar form): see
        // [`DUCKLINK_PREFIX_SCALAR_HANDLE`]. Same handler as the table
        // form but wraps its result as a VARCHAR summary.
        if handle == DUCKLINK_PREFIX_SCALAR_HANDLE {
            return self.native_ducklink_prefix_scalar(args);
        }
        // Per-row hot path: borrow the entry under the registry lock (no
        // `CallbackEntry` clone) and copy out only the Copy `dispatcher_handle`
        // plus an `Arc<str>` refcount-bump of the extension name. The historical
        // path `registry.get(handle)` cloned the whole entry -- a heap
        // allocation + string copy on EVERY dispatched row; `resolve` borrows and
        // the `Arc<str>` name handoff is an atomic refcount bump instead.
        let (dispatcher_handle, ext_name) = {
            let registry = self
                .callback_registry
                .read()
                .unwrap_or_else(|e| e.into_inner());
            match registry.resolve(handle) {
                Some(entry) if entry.kind == CallbackKind::Scalar => {
                    (entry.dispatcher_handle, entry.extension.clone())
                }
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
        let instance = match self.extensions.get_mut(&*ext_name) {
            Some(instance) => instance,
            None => {
                eprintln!(
                    "[extension-manager] dispatch_scalar could not find loaded extension '{ext_name}'"
                );
                return Err(extension_types::Duckerror::Invalidstate(format!(
                    "extension {ext_name} is not loaded"
                )));
            }
        };
        instance.dispatch_scalar(dispatcher_handle, args, ctx)
    }

    #[allow(clippy::ptr_arg)] // forwarded to a bindgen call that takes &Vec (rowbatch)
    fn dispatch_scalar_batch(
        &mut self,
        handle: u32,
        rows: &Vec<Vec<extension_types::Duckvalue>>,
        ctx: extension_runtime::Invokeinfo,
    ) -> Result<Vec<extension_types::Duckvalue>, extension_types::Duckerror> {
        // `ducklink_prefix` scalar sentinel: the core batches scalar calls
        // through this columnar entry point. Handle it per-row via the
        // shared native handler so the deferred queue reflects each
        // (alias, namespace) pair the batch declared.
        if handle == DUCKLINK_PREFIX_SCALAR_HANDLE {
            let mut out = Vec::with_capacity(rows.len());
            for row in rows {
                out.push(self.native_ducklink_prefix_scalar(row.as_slice())?);
            }
            return Ok(out);
        }
        // Resolver observability functions ride the SAME direct call-scalar-batch
        // import (no new contract): the shell-glue registers `extension_provider`
        // / `set_extension_provider` scalars with sentinel handles, which route to
        // the resolver here instead of to a resident extension.
        if handle == RESOLVER_EXPLAIN_HANDLE || handle == RESOLVER_SET_HANDLE {
            let mut out = Vec::with_capacity(rows.len());
            for row in rows {
                let arg = match row.first() {
                    Some(extension_types::Duckvalue::Text(s)) => s.clone(),
                    _ => String::new(),
                };
                let text = if handle == RESOLVER_EXPLAIN_HANDLE {
                    self.explain_resolution(&arg)
                } else {
                    self.set_forced_provider(&arg)
                };
                out.push(extension_types::Duckvalue::Text(text));
            }
            return Ok(out);
        }
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
        let instance = match self.extensions.get_mut(&*entry.extension) {
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
        // Phase 2c (@5) AT5 attach-scan sentinel range: handles minted by
        // `register_at5_scan` (from `HostState::intercept_attach`) route
        // straight to the storage-dispatch scan path — no wasm component's
        // `call-table` is involved. See `native_at5_attach_scan`.
        if (AT5_ATTACH_SCAN_HANDLE_MIN..=AT5_ATTACH_SCAN_HANDLE_MAX).contains(&handle) {
            return self.native_at5_attach_scan(handle, args);
        }
        // `ducklink_load` sentinel: STABILITY.md § 1.1's `ducklink_load(name
        // [, kind])` is surfaced as a table function registered under
        // [`DUCKLINK_LOAD_HANDLE`] in `drain_pending_registrations`. No wasm
        // component backs it — the handler runs the load orchestration
        // natively (see `native_ducklink_load` for why the standard
        // component-callback route can't work here). Analogous to the
        // resolver observability scalars in `dispatch_scalar_batch` above.
        if handle == DUCKLINK_LOAD_HANDLE {
            return self.native_ducklink_load(args);
        }
        // `ducklink_prefix` sentinel (table form): see
        // [`DUCKLINK_PREFIX_TABLE_HANDLE`]. Validates + queues the
        // (alias, namespace) pair; the actual DDL runs on the next
        // execute boundary. Returns one row
        // `(alias, namespace, macros=<count>)` where `macros` is 0 for
        // now because the drain hasn't run yet — the extension's real
        // count is a same-call return only because that host can run
        // DDL synchronously on its own connection, which the wasm core
        // can't do from inside a callback.
        if handle == DUCKLINK_PREFIX_TABLE_HANDLE {
            return self.native_ducklink_prefix_table(args);
        }
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
        let instance = match self.extensions.get_mut(&*entry.extension) {
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

    /// Native handler for `ducklink_load(name [, kind])` — invoked by
    /// `dispatch_table` when it sees the [`DUCKLINK_LOAD_HANDLE`] sentinel.
    ///
    /// Argument parsing:
    ///   * `args[0]` (positional, VARCHAR, required): extension name.
    ///   * `args[1]` (positional or named `kind`, VARCHAR, optional):
    ///     `'wasm'` (default) or `'native'`. The workspace host has only the
    ///     wasm loader path today; `'native'` returns a clean
    ///     `Duckerror::Unsupported` so the caller sees a legible message
    ///     rather than a silent no-op. Once the workspace grows a native
    ///     tier this arm becomes the same "prefer community-signed" pick
    ///     `ducklink-extension`'s `native_load_dispatch` implements.
    ///
    /// Load orchestration:
    ///   * `ensure_extension_loaded(name)` — idempotent; a re-load of a name
    ///     already in `self.extensions` short-circuits, and the returned
    ///     counts are all zero (matter-of-fact "nothing new happened").
    ///   * A `false` return means the multi-provider resolver declined,
    ///     which is surfaced as `Invalidargument` — the exact contract the
    ///     conformance suite exercises (`FROM ducklink_load('does-not-exist')`
    ///     must error cleanly).
    ///
    /// Deferred drain:
    ///   * The instance's freshly captured pending registrations are drained
    ///     here so the return-row counts are accurate. The drained data is
    ///     stashed in `self.deferred_registrations`; the extension name is
    ///     pushed to `self.deferred_drain_names`. `HostState::execute`
    ///     replays a `LOAD <name>;` on the user's next statement (which
    ///     triggers the core's normal post-LOAD `get_pending_registrations`
    ///     path and applies the deferred data to the DuckDB catalog).
    ///
    /// Return shape mirrors ducklink-extension's `WasmLoadBind`:
    /// `(name VARCHAR, path VARCHAR, scalars BIGINT, tables BIGINT,
    ///  aggregates BIGINT)`. The `path` column is NULL — the workspace
    /// resolver's on-disk path is not surfaced here yet (parity is a follow-up).
    fn native_ducklink_load(
        &mut self,
        args: &[extension_types::Duckvalue],
    ) -> Result<extension_runtime::Resultset, extension_types::Duckerror> {
        // Positional/named arg 0: extension name (VARCHAR). DuckDB passes named
        // args in order, so the first VARCHAR is the name regardless of whether
        // the caller wrote `ducklink_load('jsonfns')` or
        // `ducklink_load(name := 'jsonfns')`.
        let name = match args.first() {
            Some(extension_types::Duckvalue::Text(s)) => s.clone(),
            Some(extension_types::Duckvalue::Null) | None => {
                return Err(extension_types::Duckerror::Invalidargument(
                    "ducklink_load: missing required VARCHAR argument 'name'".into(),
                ));
            }
            Some(_) => {
                return Err(extension_types::Duckerror::Invalidargument(
                    "ducklink_load: first argument must be VARCHAR".into(),
                ));
            }
        };
        // Optional second arg: kind (VARCHAR). Matches STABILITY.md § 1.1's
        // `kind => 'wasm' | 'native'` shape; a NULL / missing value defaults
        // to 'wasm'.
        let kind = match args.get(1) {
            Some(extension_types::Duckvalue::Text(s)) => s.to_ascii_lowercase(),
            Some(extension_types::Duckvalue::Null) | None => "wasm".to_string(),
            Some(_) => {
                return Err(extension_types::Duckerror::Invalidargument(
                    "ducklink_load: second argument (kind) must be VARCHAR".into(),
                ));
            }
        };
        match kind.as_str() {
            "wasm" => {}
            "native" => {
                return Err(extension_types::Duckerror::Unsupported(
                    "ducklink_load(kind='native'): the workspace host has no native \
                     provider path yet — use kind='wasm' (the default)"
                        .into(),
                ));
            }
            other => {
                return Err(extension_types::Duckerror::Invalidargument(format!(
                    "ducklink_load: kind must be 'wasm' or 'native', got '{other}'"
                )));
            }
        }

        // `ensure_extension_loaded` returns Ok(true) on success (either fresh
        // load or already-loaded fast path), Ok(false) when the resolver
        // declined (no provider), or Err on load-time trap.
        let sanitized = sanitize_extension_name(&name);
        let loaded_ok = self.ensure_extension_loaded(&sanitized).map_err(|err| {
            extension_types::Duckerror::Internal(format!(
                "ducklink_load: failed to load '{name}': {err}"
            ))
        })?;
        if !loaded_ok {
            return Err(extension_types::Duckerror::Invalidargument(format!(
                "ducklink_load: no admissible provider for '{name}' — no manifest \
                 entry, no <extensions-dir>/{sanitized}.wasm shortcut, or the \
                 resolver declined the candidates"
            )));
        }

        // Drain what the freshly-loaded instance queued so we can report
        // scalar/table/aggregate counts in the summary row. For an
        // already-loaded ext this drain is empty (idempotent fast path
        // returned above), which is the correct "nothing new happened"
        // signal in the return row.
        let (scalars, tables, aggregates) = match self.extensions.get_mut(&sanitized) {
            Some(instance) => {
                let drained = instance.drain_pending();
                let counts = (
                    drained.scalars.len(),
                    drained.tables.len(),
                    drained.aggregates.len(),
                );
                // Stash for the deferred core-side drain (see
                // `deferred_registrations` field doc + `HostState::execute`).
                self.deferred_registrations.append(drained);
                counts
            }
            None => (0, 0, 0),
        };

        // Schedule an idempotent `LOAD <name>;` on the next user statement.
        // Dedup: multiple `ducklink_load('x')` calls before a drain shouldn't
        // pile up N LOAD-x driver statements.
        if !self.deferred_drain_names.iter().any(|n| n == &sanitized) {
            self.deferred_drain_names.push(sanitized.clone());
        }

        eprintln!(
            "[extension-manager] ducklink_load('{sanitized}') -> \
             scalars={scalars}, tables={tables}, aggregates={aggregates} \
             (deferred core drain scheduled)"
        );
        let row: Vec<extension_types::Duckvalue> = vec![
            extension_types::Duckvalue::Text(sanitized),
            // `path` is intentionally NULL — the workspace resolver's
            // artifact path isn't surfaced through this API yet (parity
            // with ducklink-extension's `path` column is a follow-up).
            extension_types::Duckvalue::Null,
            extension_types::Duckvalue::Int64(scalars as i64),
            extension_types::Duckvalue::Int64(tables as i64),
            extension_types::Duckvalue::Int64(aggregates as i64),
        ];
        Ok(vec![row])
    }

    /// Consumes any pending `ducklink_load(name)` drain requests. Called by
    /// [`HostState::execute`] before running the user's next statement so an
    /// idempotent `LOAD <name>;` triggers the core's post-LOAD drain and the
    /// stashed `deferred_registrations` reach the DuckDB catalog.
    fn take_deferred_drain_names(&mut self) -> Vec<String> {
        std::mem::take(&mut self.deferred_drain_names)
    }

    /// Shared body of the `ducklink_prefix(alias, namespace)` sentinel
    /// intercepts. Validates the identifiers and queues the pair for the
    /// next-execute deferred drain. Returns the sanitized `(alias,
    /// namespace)` pair so the caller can shape its own return row.
    ///
    /// The extension's [`run_ducklink_prefix`] runs the DDL synchronously
    /// on its own connection and returns a real macros-created count. The
    /// workspace host can't: this native handler runs inside a
    /// `dispatch_scalar`/`dispatch_table` callback and the core wasm store
    /// is mid-call — re-entering `call_execute` would deadlock the core
    /// mutex and violate wasmtime store re-entrancy (same shape as the
    /// `native_ducklink_load` deferred-drain rationale). The macros count
    /// is therefore surfaced as 0 from the same call; the real DDL runs
    /// on the next `HostState::execute` boundary
    /// ([`HostState::flush_deferred_prefix_declarations`]) so `<alias>.<fn>`
    /// resolves in every subsequent statement.
    fn native_ducklink_prefix_common(
        &mut self,
        args: &[extension_types::Duckvalue],
    ) -> Result<(String, String), extension_types::Duckerror> {
        let alias = match args.first() {
            Some(extension_types::Duckvalue::Text(s)) => s.clone(),
            Some(extension_types::Duckvalue::Null) | None => {
                return Err(extension_types::Duckerror::Invalidargument(
                    "ducklink_prefix: missing required VARCHAR argument 'alias'".into(),
                ));
            }
            Some(_) => {
                return Err(extension_types::Duckerror::Invalidargument(
                    "ducklink_prefix: first argument (alias) must be VARCHAR".into(),
                ));
            }
        };
        let namespace = match args.get(1) {
            Some(extension_types::Duckvalue::Text(s)) => s.clone(),
            Some(extension_types::Duckvalue::Null) | None => {
                return Err(extension_types::Duckerror::Invalidargument(
                    "ducklink_prefix: missing required VARCHAR argument 'namespace'".into(),
                ));
            }
            Some(_) => {
                return Err(extension_types::Duckerror::Invalidargument(
                    "ducklink_prefix: second argument (namespace) must be VARCHAR".into(),
                ));
            }
        };
        if !is_safe_prefix_identifier(&alias) || !is_safe_prefix_identifier(&namespace) {
            return Err(extension_types::Duckerror::Invalidargument(format!(
                "ducklink_prefix: alias and namespace must match [A-Za-z0-9_]+ \
                 (got alias='{alias}', namespace='{namespace}')"
            )));
        }
        // Dedup within the pending queue — repeated
        // `ducklink_prefix('c','main')` calls in the same statement
        // shouldn't pile up N replays on the next execute.
        if !self
            .deferred_prefix_declarations
            .iter()
            .any(|(a, n)| a == &alias && n == &namespace)
        {
            self.deferred_prefix_declarations
                .push((alias.clone(), namespace.clone()));
        }
        eprintln!(
            "[extension-manager] ducklink_prefix('{alias}', '{namespace}') queued \
             (deferred drain scheduled on next execute)"
        );
        Ok((alias, namespace))
    }

    /// Native handler for `FROM ducklink_prefix('alias','namespace')` —
    /// invoked by `dispatch_table` on the [`DUCKLINK_PREFIX_TABLE_HANDLE`]
    /// sentinel. Returns one row `(alias, namespace, macros BIGINT)`. The
    /// `macros` count is 0 for this call — see
    /// [`native_ducklink_prefix_common`] for the deferred-execution
    /// rationale; a subsequent query against `information_schema.tables`
    /// or `duckdb_functions()` will show the created macros once the
    /// next `HostState::execute` boundary drains the queue.
    fn native_ducklink_prefix_table(
        &mut self,
        args: &[extension_types::Duckvalue],
    ) -> Result<extension_runtime::Resultset, extension_types::Duckerror> {
        let (alias, namespace) = self.native_ducklink_prefix_common(args)?;
        let row: Vec<extension_types::Duckvalue> = vec![
            extension_types::Duckvalue::Text(alias),
            extension_types::Duckvalue::Text(namespace),
            extension_types::Duckvalue::Int64(0),
        ];
        Ok(vec![row])
    }

    /// Native handler for `SELECT ducklink_prefix('alias','namespace')` —
    /// invoked by `dispatch_scalar` / `dispatch_scalar_batch` on the
    /// [`DUCKLINK_PREFIX_SCALAR_HANDLE`] sentinel. Returns a VARCHAR
    /// summary shaped like the extension's:
    /// `"alias='c' namespace='main' macros=0 (deferred)"`.
    fn native_ducklink_prefix_scalar(
        &mut self,
        args: &[extension_types::Duckvalue],
    ) -> Result<extension_types::Duckvalue, extension_types::Duckerror> {
        let (alias, namespace) = self.native_ducklink_prefix_common(args)?;
        Ok(extension_types::Duckvalue::Text(format!(
            "alias='{alias}' namespace='{namespace}' macros=0 (deferred)"
        )))
    }

    /// Move the pending `(alias, namespace)` pairs out so
    /// [`HostState::flush_deferred_prefix_declarations`] can drive the
    /// DDL on the idle core without holding the extension-manager lock
    /// across `call_execute`.
    fn take_deferred_prefix_declarations(&mut self) -> Vec<(String, String)> {
        std::mem::take(&mut self.deferred_prefix_declarations)
    }

    // --- 3.1.0 additive minor: streaming + filter-pushdown table-fn dispatch ---
    // Route the GLOBAL callback handle (carried in the C++ TableFunction) to the
    // owning component instance + its component-local dispatcher handle, exactly
    // like dispatch_table routes call-table.

    fn registered_filterable_tables(&self) -> Vec<reg::FilterableTableReg> {
        self.filterable_tables.clone()
    }

    fn dispatch_table_open_filtered(
        &mut self,
        handle: u32,
        args: &[extension_types::Duckvalue],
        projection: &[u32],
        filters: &[ducklink_runtime::extension::TableFilter],
    ) -> Result<ducklink_runtime::extension::TableOpenResult, extension_types::Duckerror> {
        let entry = self
            .lookup_callback(handle, CallbackKind::Table)
            .ok_or_else(|| {
                extension_types::Duckerror::Invalidstate(format!(
                    "unknown filterable-table handle {handle}"
                ))
            })?;
        let instance = self.extensions.get_mut(&*entry.extension).ok_or_else(|| {
            extension_types::Duckerror::Invalidstate(format!(
                "extension {} is not loaded",
                entry.extension
            ))
        })?;
        instance.table_open_filtered(entry.dispatcher_handle, args, projection, filters)
    }

    fn dispatch_table_next(
        &mut self,
        handle: u32,
        cursor: u32,
        max_rows: u32,
    ) -> Result<extension_runtime::Resultset, extension_types::Duckerror> {
        let entry = self
            .lookup_callback(handle, CallbackKind::Table)
            .ok_or_else(|| {
                extension_types::Duckerror::Invalidstate(format!(
                    "unknown filterable-table handle {handle}"
                ))
            })?;
        let instance = self.extensions.get_mut(&*entry.extension).ok_or_else(|| {
            extension_types::Duckerror::Invalidstate(format!(
                "extension {} is not loaded",
                entry.extension
            ))
        })?;
        instance.table_next(entry.dispatcher_handle, cursor, max_rows)
    }

    fn dispatch_table_close(
        &mut self,
        handle: u32,
        cursor: u32,
    ) -> Result<bool, extension_types::Duckerror> {
        let entry = self
            .lookup_callback(handle, CallbackKind::Table)
            .ok_or_else(|| {
                extension_types::Duckerror::Invalidstate(format!(
                    "unknown filterable-table handle {handle}"
                ))
            })?;
        let instance = self.extensions.get_mut(&*entry.extension).ok_or_else(|| {
            extension_types::Duckerror::Invalidstate(format!(
                "extension {} is not loaded",
                entry.extension
            ))
        })?;
        instance.table_close(entry.dispatcher_handle, cursor)
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
        let instance = match self.extensions.get_mut(&*entry.extension) {
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
        let instance = match self.extensions.get_mut(&*entry.extension) {
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
        let instance = match self.extensions.get_mut(&*entry.extension) {
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
            .read()
            .unwrap_or_else(|e| e.into_inner());
        registry.get(handle).filter(|entry| entry.kind == kind)
    }

    fn ensure_extension_loaded(&mut self, name: &str) -> wasmtime::Result<bool> {
        let sanitized = sanitize_extension_name(name);
        if self.extensions.contains_key(&sanitized) {
            return Ok(true);
        }

        // Phase D: per-sub-extension `compose:dynlink` bridge branch. If the
        // requested name is present in `sub_ext_loader.sub_ext_bridge_paths`,
        // materialize the composed provider (once) and load the configured
        // bridge wasm instead of taking the flat resolver path. This runs
        // BEFORE `resolve_provider_artifact` so a sub-ext bridge stored
        // outside `<extensions-dir>` (e.g. under a `postgis-core-ducklink-bridge`
        // checkout) doesn't need to be symlinked into the extensions dir.
        // Falls through to the standard resolver when neither the bridge map
        // nor a plan is configured for `sanitized` — preserves the existing
        // LOAD semantics for every non-sub-ext extension bit-identically.
        let artifact = if self.sub_ext_loader.has_bridge(&sanitized) {
            // Compose + register the composed provider under
            // `<sanitized>-composed`. Idempotent; a re-issued LOAD hits the
            // materialized guard and no-ops.
            match self.sub_ext_loader.materialize_sub_ext_provider(&sanitized) {
                Ok(provider_id) => eprintln!(
                    "[sub-ext] '{sanitized}' composed provider registered as '{provider_id}'"
                ),
                Err(err) => {
                    eprintln!(
                        "[sub-ext] '{sanitized}' materialization failed: {err}; skipping load request"
                    );
                    return Ok(false);
                }
            }
            let bridge = self
                .sub_ext_loader
                .bridge_path(&sanitized)
                .expect("has_bridge guarded above")
                .to_path_buf();
            if !bridge.exists() {
                eprintln!(
                    "[sub-ext] bridge wasm for '{sanitized}' not found at {}; skipping load request",
                    bridge.display()
                );
                return Ok(false);
            }
            eprintln!(
                "[sub-ext] '{sanitized}' bridge wasm: {}",
                bridge.display()
            );
            bridge
        } else {
            // Multi-provider resolution (design A): pick a certified provider via the
            // resolver candidate pipeline instead of the bare filename shortcut. An
            // extension absent from the manifest falls back to the filename.
            let artifact = match self.resolve_provider_artifact(&sanitized) {
                Ok(p) => p,
                Err(reason) => {
                    eprintln!("[resolver] no admissible provider for '{sanitized}': {reason}");
                    return Ok(false);
                }
            };
            if !artifact.exists() {
                eprintln!(
                    "[extension-manager] resolved artifact for '{sanitized}' not found at {}; skipping load request",
                    artifact.display()
                );
                return Ok(false);
            }
            artifact
        };

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
        let sibling = self.sibling.clone();
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
            // Grant the extension access to the absolute cache-root paths from
            // DUCKLINK_LOCAL_CACHE / DUCKLINK_GLOBAL_CACHE. The cache
            // extension resolves those verbatim; without a matching preopen
            // (guest name == absolute host path) WASI rejects
            // `<abs>/objects` etc. with `No such file or directory`.
            attach_cache_env_preopens(&mut builder);
            // Grant sqlitewasm access to ATTACH DSN paths so it can self-persist
            // through wasivfs (sqlite-lib SPI) rather than shuttling bytes across
            // the wasm boundary. Preopens cwd (as ".") so relative DSNs Just
            // Work; DUCKLINK_SQLITEWASM_DATADIR (absolute path, guest name ==
            // abspath) extends the reach to any dir the operator names — mirrors
            // the `DUCKLINK_LOCAL_CACHE` shape. Skipped for extensions other
            // than sqlitewasm to keep the fs grant scoped.
            attach_sqlitewasm_preopens(&mut builder, &extension_name);
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
                    sibling,
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
            let collations_captured = instance.take_pending_collations();
            let pragmas_captured = instance.take_pending_pragmas();
            // 2.3.0 / v3: capture this extension's parser extensions, so the core
            // can offer rejected statements to them (via parser-host).
            let parsers_captured = instance.take_pending_parsers();
            let optimizers_captured = instance.take_pending_optimizers();
            // 3.1.0 additive minor: capture this extension's filterable streaming
            // table functions, so the core can register a real filter-pushdown
            // TableFunction (via table-stream-host.filterable-table-list) and route
            // its open/next/close back to this component.
            for ft in instance.take_pending_filterable_tables() {
                eprintln!(
                    "[extension-manager] filterable table fn '{}' -> extension '{}' (global-handle={}, args={}, cols={})",
                    ft.name,
                    ft.extension,
                    ft.callback_handle,
                    ft.arguments.len(),
                    ft.columns.len()
                );
                self.filterable_tables.push(ft);
            }
            for parser in &parsers_captured {
                eprintln!(
                    "[extension-manager] parser '{}' -> extension '{}' (callback={})",
                    parser.name, parser.extension, parser.callback_handle
                );
                self.parsers.insert(
                    parser.name.clone(),
                    (parser.extension.clone(), parser.callback_handle),
                );
            }
            for opt in &optimizers_captured {
                eprintln!(
                    "[extension-manager] optimizer rule '{}' -> extension '{}' (callback={})",
                    opt.rule_name, opt.extension, opt.callback_handle
                );
                self.optimizers.insert(
                    opt.rule_name.clone(),
                    (opt.extension.clone(), opt.callback_handle),
                );
            }
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
        eprintln!(
            "[extension-manager] extension '{loaded_name}' loaded successfully and ready for registrations"
        );
        // Phase 4 follow-up (FU4): if a `SiblingState` is wired, record the
        // successful load. `sibling_ensure_slot` reads this list to choose a
        // valid extension name for the single LOAD it issues on the sibling
        // (which primes the sibling's `get_pending_registrations` to serve
        // `SiblingState::replay_archive`).
        if let Some(sibling) = self.sibling.as_ref() {
            sibling.record_extension_loaded(&loaded_name);
        }
        Ok(true)
    }

    fn is_loaded(&self, name: &str) -> bool {
        let sanitized = sanitize_extension_name(name);
        self.extensions.contains_key(&sanitized)
    }

    fn drain_pending_registrations(&mut self) -> PendingRegistrationsData {
        let mut aggregated = PendingRegistrationsData::default();
        // Prepend anything the native `ducklink_load(name)` handler stashed
        // in a prior call: those registrations belong to an extension already
        // in `self.extensions`, but the handler drained them off the instance
        // to count them for its return row. Draining here from the same
        // instance would find nothing; this prepend delivers them to the
        // core catalog on the deferred `LOAD <name>;` that `HostState::execute`
        // replays. See `deferred_registrations` field doc.
        aggregated.append(std::mem::take(&mut self.deferred_registrations));
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
        // Phase 3 (@5 host-import wiring): capture secret TYPE + PROVIDER
        // backings so a future `CREATE SECRET (TYPE ..., PROVIDER ..., k v)`
        // interceptor can route into the backing component's
        // `secret-dispatch::create-secret` export. Mirrors the storage_backends
        // pattern above: `secrets` never crosses the core-hook boundary
        // (secrets are consumed on the host side only, via `create_secret`),
        // so we record the mapping here before the field is dropped downstream.
        for secret in &aggregated.secrets {
            let provider_txt = secret.provider.as_deref().unwrap_or("<default>");
            eprintln!(
                "[extension-manager] secret backend '{}'/'{provider_txt}' -> extension '{}' (callback={})",
                secret.type_name, secret.extension, secret.callback_handle,
            );
            self.secret_backends.insert(
                (secret.type_name.clone(), secret.provider.clone()),
                (secret.extension.clone(), secret.callback_handle),
            );
        }
        // Phase 3 (@5 host-import wiring): capture option declarations so a
        // future `SET <name> = <value>` interceptor can consult the map and
        // forward the change to the declaring component's
        // `settings-dispatch::on-setting-set` export. The `SettingReg` still
        // rides through `PendingRegistrationsData::settings` for the core's
        // OPTION registration path; the registry is the SECOND consumer.
        for setting in &aggregated.settings {
            eprintln!(
                "[extension-manager] setting option '{}' -> extension '{}' (type={}, scope={})",
                setting.name, setting.extension, setting.ty, setting.scope,
            );
            self.settings_registry.insert(
                setting.name.clone(),
                (setting.extension.clone(), setting.clone()),
            );
        }
        // One-shot: append the synthetic `ducklink_load` table function
        // (STABILITY.md § 1.1). The `callback_handle` is the reserved
        // [`DUCKLINK_LOAD_HANDLE`] sentinel; `dispatch_table` intercepts it
        // and calls `native_ducklink_load` instead of routing to a component.
        //
        // The `apply_function_prefixes`-plus-PIN shadowing that used to live
        // right before this block is gone with the prefix__name retirement
        // (workspace commit a048b7a). If/when the schema-based prefix.name
        // model lands, this injection stays where it is; the model runs
        // out-of-band via `ducklink_prefix(...)`.
        if !self.injected_ducklink_load {
            self.injected_ducklink_load = true;
            aggregated.tables.push(reg::TableReg {
                extension: "ducklink".to_string(),
                name: "ducklink_load".to_string(),
                // The `duckdb-wasi` core registers every `funcarg` as a
                // POSITIONAL required parameter (`duckdb_table_function_add_parameter`
                // — no distinction between named + optional). Only the
                // required `name` argument is surfaced today so
                // `FROM ducklink_load('jsonfns')` binds. STABILITY.md § 1.1
                // still commits `kind => 'wasm' | 'native'`; the wasm-core
                // table-fn registration WIT needs a `named_parameters` field
                // before we can surface it here without turning
                // `ducklink_load('jsonfns')` into a bind error. Tracked as a
                // follow-up — the intercept in `native_ducklink_load`
                // already handles a second VARCHAR arg once the core
                // starts forwarding it.
                arguments: vec![reg::FuncArg {
                    name: Some("name".to_string()),
                    logical: reg::LogicalType::Text,
                }],
                columns: vec![
                    reg::ColumnDef {
                        name: "name".to_string(),
                        logical: reg::LogicalType::Text,
                    },
                    reg::ColumnDef {
                        name: "path".to_string(),
                        logical: reg::LogicalType::Text,
                    },
                    reg::ColumnDef {
                        name: "scalars".to_string(),
                        logical: reg::LogicalType::Int64,
                    },
                    reg::ColumnDef {
                        name: "tables".to_string(),
                        logical: reg::LogicalType::Int64,
                    },
                    reg::ColumnDef {
                        name: "aggregates".to_string(),
                        logical: reg::LogicalType::Int64,
                    },
                ],
                callback_handle: DUCKLINK_LOAD_HANDLE,
                options: None,
            });
            eprintln!(
                "[extension-manager] injected synthetic `ducklink_load` table function \
                 (STABILITY.md § 1.1) via sentinel handle {DUCKLINK_LOAD_HANDLE:#x}"
            );
        }

        // One-shot: append the synthetic `ducklink_prefix` TF + scalar
        // (STABILITY.md § 1.1). Both are backed by the same native handler
        // (`native_ducklink_prefix_common`) which validates the args and
        // queues the work for the next-execute deferred drain. See the
        // sentinel-handle docs on [`DUCKLINK_PREFIX_TABLE_HANDLE`] for why
        // the DDL cannot run in-band.
        if !self.injected_ducklink_prefix {
            self.injected_ducklink_prefix = true;
            // Table form: `FROM ducklink_prefix('c','main')` yields one row
            // `(alias, namespace, macros BIGINT)` mirroring the extension's
            // `DucklinkPrefix` VTab.
            aggregated.tables.push(reg::TableReg {
                extension: "ducklink".to_string(),
                name: "ducklink_prefix".to_string(),
                arguments: vec![
                    reg::FuncArg {
                        name: Some("alias".to_string()),
                        logical: reg::LogicalType::Text,
                    },
                    reg::FuncArg {
                        name: Some("namespace".to_string()),
                        logical: reg::LogicalType::Text,
                    },
                ],
                columns: vec![
                    reg::ColumnDef {
                        name: "alias".to_string(),
                        logical: reg::LogicalType::Text,
                    },
                    reg::ColumnDef {
                        name: "namespace".to_string(),
                        logical: reg::LogicalType::Text,
                    },
                    reg::ColumnDef {
                        name: "macros".to_string(),
                        logical: reg::LogicalType::Int64,
                    },
                ],
                callback_handle: DUCKLINK_PREFIX_TABLE_HANDLE,
                options: None,
            });
            // Scalar form: `SELECT ducklink_prefix('c','main')` returns a
            // VARCHAR summary. Same name as the TF — DuckDB's binder
            // disambiguates by call site (`SELECT foo(...)` vs `FROM
            // foo(...)`), matching the extension's dual-registration.
            aggregated.scalars.push(reg::ScalarReg {
                extension: "ducklink".to_string(),
                name: "ducklink_prefix".to_string(),
                arguments: vec![
                    reg::FuncArg {
                        name: Some("alias".to_string()),
                        logical: reg::LogicalType::Text,
                    },
                    reg::FuncArg {
                        name: Some("namespace".to_string()),
                        logical: reg::LogicalType::Text,
                    },
                ],
                returns: reg::LogicalType::Text,
                callback_handle: DUCKLINK_PREFIX_SCALAR_HANDLE,
                options: None,
            });
            eprintln!(
                "[extension-manager] injected synthetic `ducklink_prefix` TF+scalar \
                 (STABILITY.md § 1.1) via sentinel handles \
                 table={DUCKLINK_PREFIX_TABLE_HANDLE:#x} \
                 scalar={DUCKLINK_PREFIX_SCALAR_HANDLE:#x}"
            );
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
    /// wasi:http host context (see the module-level `add_wasi_http_to_linker`
    /// import). The CLI itself doesn't call wasi:http today; this is present so
    /// the CLI-linker's `add_only_http_to_linker_sync` has a `WasiHttpView`
    /// impl to project. Extensions loaded through this host route through
    /// `ExtensionStoreState` (which carries its own `WasiHttpCtx`) — this is
    /// only for the front-end store.
    wasi_http: WasiHttpCtx,
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
    /// nested-exec Direction-1 §5.(b.1): shared sibling-core state, populated
    /// with the primary's opened DB path on the first `open` call so a later
    /// extension `nested_exec` can materialize the sibling. `None` = the
    /// harness did not wire nested-exec (narrow paths).
    sibling: Option<Arc<SiblingState>>,
    /// Phase 2 (@5): aliases previously ATTACHed against a storage-capable
    /// extension. Maps `<alias>` → `(extension-id, catalog-handle,
    /// callback-handle, table-columns-keyed-by-table-name)`. Populated by the
    /// ATTACH intercept in `execute`; consulted by the write intercept.
    attached_aliases: HashMap<String, AttachedForeignCatalog>,
}

/// Metadata the ATTACH intercept records for an `<alias>` bound to a storage
/// extension. See `HostState::attached_aliases`.
#[derive(Clone)]
pub(crate) struct AttachedForeignCatalog {
    pub extension: String,
    pub catalog_handle: u32,
    pub callback_handle: u32,
    pub type_name: String,
    pub tables: Vec<String>,
    /// Phase 2c: per-table AT5 attach-scan callback handles allocated at
    /// `intercept_attach` time. Keyed by table name; the value is the handle
    /// stashed in the core catalog under `__<alias>_<table>` via
    /// `database.register-table-function` (see `ExtensionManager::at5_scan_targets`
    /// for the reverse mapping the manager consults on dispatch).
    pub table_callbacks: HashMap<String, u32>,
    /// AT5 write-back destination: the resolved local file path the ATTACH DSN
    /// pointed at, or None when the DSN wasn't a file (connection strings,
    /// remote URIs). After every INSERT/UPDATE/DELETE dispatch the host calls
    /// `dispatch_storage_serialize` and, if the backend returns bytes AND this
    /// path is set, writes the blob back so the mutation persists on disk.
    /// See `intercept_write` for the wiring.
    pub dsn_path: Option<PathBuf>,
    /// AT5 write-back suppression: an ATTACH made with `READ_ONLY` (or its
    /// synonyms) skips the write-back path so a read-only mount stays read-
    /// only. The write-intercept still rejects mutations at parse-time; this
    /// belt-and-braces guard covers any future codepath that bypasses that.
    pub read_only: bool,
    /// Bug 4b: an extension that advertises `writes_persist_directly = true`
    /// (currently sqlitewasm via sqlite-lib + wasivfs) is authoritative for
    /// durability — the host skips its own serialize + rewrite. There's no
    /// host-side rusqlite connection anymore: the write goes wasm-side
    /// through the SPI, wasivfs turns it into wasi:filesystem calls, and
    /// the extension loader's per-extension WasiCtx preopens the target
    /// dir (see `attach_sqlitewasm_preopens`). The `writes_persist_directly`
    /// probe result is cached here so `at5_write_back` can short-circuit
    /// without re-dispatching. `false` = probe returned false / errored /
    /// wasn't run (read-only attach); the legacy full-serialize write-back
    /// path applies.
    pub writes_persist_directly: bool,
}

/// Phase 2c (@5): routing target for a single AT5 attach-scan callback
/// handle. The manager consults this via `at5_scan_targets` when
/// `dispatch_table` receives a handle in the `AT5_ATTACH_SCAN_HANDLE_*`
/// range. Owning the (extension, catalog_handle) pair here (rather than
/// re-looking it up through `attached_aliases` on HostState — which
/// `dispatch_table` doesn't have a reference to) keeps the manager
/// self-contained.
#[derive(Debug, Clone)]
struct At5ScanTarget {
    /// Which loaded extension owns the storage backend for this scan.
    extension: String,
    /// The storage-dispatch callback handle the extension declared in
    /// `register-storage`. Passed as the `handle` argument to every
    /// `storage-dispatch.*` re-entry.
    storage_handle: u32,
    /// The catalog handle returned by the extension's `storage-attach`.
    catalog_handle: u32,
    /// The table name to scan.
    table: String,
}

impl WasiView for HostState {
    fn ctx(&mut self) -> WasiCtxView<'_> {
        WasiCtxView {
            ctx: &mut self.wasi,
            table: &mut self.table,
        }
    }
}

impl WasiHttpView for HostState {
    fn http(&mut self) -> WasiHttpCtxView<'_> {
        WasiHttpCtxView {
            ctx: &mut self.wasi_http,
            table: &mut self.table,
            hooks: Default::default(),
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
        // `ducklink-scalars` ships the two always-available scalars
        // committed in `ducklink-extension/STABILITY.md § 1.1`
        // (`ducklink_version`, `ducklink_help`). Autoloading it matches
        // the extension's behaviour and satisfies the two rows the
        // conformance suite expects for those names.
        let spec = std::env::var("DUCKLINK_AUTOLOAD")
            .unwrap_or_else(|_| String::from("jsonfns,ducklink_scalars"));
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
        // Bring up the `ducklink.*` discovery schema on the freshly-opened
        // connection. Runs after the autoload pass so a `SELECT * FROM
        // ducklink.modules` in the very first user statement resolves.
        self.create_ducklink_schema(&handle);
    }

    /// Create the public `ducklink.*` discovery schema (STABILITY.md § 1.2) on
    /// `handle`. Mirrors the extension's `create_ducklink_schema` shape-for-
    /// shape (same view names, same column names, same column types, same
    /// column order). Idempotent (`CREATE SCHEMA IF NOT EXISTS`, `CREATE OR
    /// REPLACE VIEW`); run once at connection open.
    ///
    /// Each view is a zero-row projection of typed NULLs. The workspace host
    /// does not yet expose the runtime state the native extension's `WasmXxx`
    /// TFs read from (loaded-component list, catalog, on-disk cache, event
    /// log), so an honest zero-row view of the correct SHAPE is what we can
    /// commit to today — enough to make `SELECT * FROM ducklink.<name>`
    /// resolve and to satisfy the shape assertions in the cross-host
    /// conformance suite (`conformance/scripts/02-*.sql`).
    ///
    /// `ducklink.prefixes` is a persistent TABLE (not a view) minted here
    /// as part of the discovery schema so `information_schema.tables
    /// WHERE table_schema='ducklink'` surfaces it (conformance script 02)
    /// and so [`HostState::flush_deferred_prefix_declarations`] has a place
    /// to `INSERT OR REPLACE` each `ducklink_prefix(alias, namespace)`
    /// declaration. `PREFIX(alias, namespace)` is registered here too so
    /// `SELECT PREFIX(...)` binds through DuckDB's macro path to the
    /// `ducklink_prefix` scalar sentinel — the extension registers it the
    /// same way in `reg_duckdb.rs`.
    ///
    /// Non-fatal: DDL failures are logged and skipped rather than aborting
    /// the connection.
    fn create_ducklink_schema(&mut self, handle: &ResourceAny) {
        // `ducklink.search(query)` is a MACRO (takes a bound query argument)
        // — the other eight are VIEWs. Statements are executed one at a time
        // because the CLI's `execute` boundary is a single-statement call.
        //
        // The `CREATE OR REPLACE MACRO PREFIX(alias, namespace) AS
        // ducklink_prefix(...)` DDL is HOISTED out of this list and handled
        // below — it depends on the synthetic `ducklink_prefix` scalar being
        // present in the catalog. That scalar is registered lazily by
        // `ExtensionManager::drain_pending_registrations`, which only runs
        // when the wasm core requests pending registrations (post-LOAD
        // handshake). On a lean host where no autoloaded extension is
        // present (e.g. `jsonfns` + `ducklink_scalars` artifacts missing on
        // a fresh worktree), `drain_pending_registrations` never fires, and
        // the DuckDB binder rejects the macro body with "Scalar Function
        // with name ducklink_prefix does not exist!". Gate the DDL below on
        // an explicit `duckdb_functions()` probe so that noisy failure never
        // hits the AT5 e2e tests' captured stderr.
        const DDL: &[&str] = &[
            "CREATE SCHEMA IF NOT EXISTS ducklink",
            // prefixes — persistent table (NOT a view). Populated by
            // `HostState::flush_deferred_prefix_declarations` after each
            // `ducklink_prefix(alias, namespace)` call.
            "CREATE TABLE IF NOT EXISTS ducklink.prefixes ( \
                alias VARCHAR PRIMARY KEY, \
                namespace VARCHAR NOT NULL)",
            // modules — 11 cols
            "CREATE OR REPLACE VIEW ducklink.modules AS \
             SELECT CAST(NULL AS VARCHAR) AS name, \
                    CAST(NULL AS VARCHAR) AS version, \
                    CAST(NULL AS VARCHAR) AS description, \
                    CAST(NULL AS VARCHAR) AS categories, \
                    CAST(NULL AS BOOLEAN) AS loaded, \
                    CAST(NULL AS BOOLEAN) AS native_available, \
                    CAST(NULL AS INTEGER) AS scalars, \
                    CAST(NULL AS INTEGER) AS tables, \
                    CAST(NULL AS INTEGER) AS aggregates, \
                    CAST(NULL AS VARCHAR) AS capabilities, \
                    CAST(NULL AS BOOLEAN) AS compatible \
             WHERE FALSE",
            // functions — 6 cols
            "CREATE OR REPLACE VIEW ducklink.functions AS \
             SELECT CAST(NULL AS VARCHAR) AS module, \
                    CAST(NULL AS VARCHAR) AS name, \
                    CAST(NULL AS VARCHAR) AS kind, \
                    CAST(NULL AS VARCHAR) AS arguments, \
                    CAST(NULL AS VARCHAR) AS returns, \
                    CAST(NULL AS BOOLEAN) AS loaded \
             WHERE FALSE",
            // host — 2 cols (single-row in the extension; zero-row placeholder here)
            "CREATE OR REPLACE VIEW ducklink.host AS \
             SELECT CAST(NULL AS VARCHAR) AS wasm_abi, \
                    CAST(NULL AS VARCHAR) AS duckdb_version \
             WHERE FALSE",
            // host_capabilities — 3 cols
            "CREATE OR REPLACE VIEW ducklink.host_capabilities AS \
             SELECT CAST(NULL AS VARCHAR) AS name, \
                    CAST(NULL AS BOOLEAN) AS available, \
                    CAST(NULL AS VARCHAR) AS detail \
             WHERE FALSE",
            // cache — 5 cols
            "CREATE OR REPLACE VIEW ducklink.cache AS \
             SELECT CAST(NULL AS VARCHAR) AS digest, \
                    CAST(NULL AS VARCHAR) AS name, \
                    CAST(NULL AS BIGINT) AS bytes, \
                    CAST(NULL AS TIMESTAMP) AS modified, \
                    CAST(NULL AS VARCHAR) AS path \
             WHERE FALSE",
            // module_compatibility — 6 cols
            "CREATE OR REPLACE VIEW ducklink.module_compatibility AS \
             SELECT CAST(NULL AS VARCHAR) AS module, \
                    CAST(NULL AS VARCHAR) AS module_generation, \
                    CAST(NULL AS VARCHAR) AS host_generation, \
                    CAST(NULL AS VARCHAR) AS lifecycle, \
                    CAST(NULL AS BOOLEAN) AS runnable, \
                    CAST(NULL AS BOOLEAN) AS selected \
             WHERE FALSE",
            // events — 5 cols
            "CREATE OR REPLACE VIEW ducklink.events AS \
             SELECT CAST(NULL AS BIGINT) AS seq, \
                    CAST(NULL AS TIMESTAMP) AS ts, \
                    CAST(NULL AS VARCHAR) AS kind, \
                    CAST(NULL AS VARCHAR) AS module, \
                    CAST(NULL AS VARCHAR) AS detail \
             WHERE FALSE",
            // docs — 9 cols
            "CREATE OR REPLACE VIEW ducklink.docs AS \
             SELECT CAST(NULL AS VARCHAR) AS module, \
                    CAST(NULL AS VARCHAR) AS function, \
                    CAST(NULL AS VARCHAR) AS kind, \
                    CAST(NULL AS VARCHAR) AS signature, \
                    CAST(NULL AS VARCHAR) AS summary, \
                    CAST(NULL AS VARCHAR) AS description, \
                    CAST(NULL AS VARCHAR) AS example, \
                    CAST(NULL AS VARCHAR) AS tags, \
                    CAST(NULL AS BOOLEAN) AS loaded \
             WHERE FALSE",
            // search(query) — 7 cols, MACRO (takes a bound query argument)
            "CREATE OR REPLACE MACRO ducklink.search(query) AS TABLE \
             SELECT CAST(NULL AS VARCHAR) AS module, \
                    CAST(NULL AS VARCHAR) AS function, \
                    CAST(NULL AS VARCHAR) AS kind, \
                    CAST(NULL AS VARCHAR) AS signature, \
                    CAST(NULL AS VARCHAR) AS summary, \
                    CAST(NULL AS VARCHAR) AS tags, \
                    CAST(NULL AS BIGINT) AS score \
             WHERE FALSE",
        ];
        for sql in DDL {
            let res = self.with_core(|core| {
                core.with_database(|guest, store| guest.call_execute(store, handle.clone(), sql))
            });
            match res {
                Ok(Ok(_)) => {}
                Ok(Err(err)) => eprintln!(
                    "[ducklink] discovery-view DDL failed: {}: {}",
                    sql,
                    core_duckerror_message(err)
                ),
                Err(trap) => eprintln!(
                    "[ducklink] discovery-view DDL trapped: {}: {}",
                    sql, trap
                ),
            }
        }
        // PREFIX(alias, namespace) — shorter macro that delegates to the
        // `ducklink_prefix` scalar sentinel (STABILITY.md § 1.1). Mirrors
        // the extension's `reg_duckdb.rs` registration at `ducklink_load`
        // time. Skipped when the scalar sentinel is not yet in the core
        // catalog (see the block-comment on the DDL constant above) — the
        // macro is a UX convenience; its absence downgrades cleanly to
        // `SELECT ducklink_prefix(...)` calls, which are the STABILITY.md
        // committed shape anyway.
        const PREFIX_MACRO_DDL: &str =
            "CREATE OR REPLACE MACRO PREFIX(alias, namespace) AS \
             ducklink_prefix(alias, namespace)";
        if self.ducklink_prefix_scalar_registered(handle) {
            let res = self.with_core(|core| {
                core.with_database(|guest, store| {
                    guest.call_execute(store, handle.clone(), PREFIX_MACRO_DDL)
                })
            });
            match res {
                Ok(Ok(_)) => {}
                Ok(Err(err)) => eprintln!(
                    "[ducklink] discovery-view DDL failed: {}: {}",
                    PREFIX_MACRO_DDL,
                    core_duckerror_message(err)
                ),
                Err(trap) => eprintln!(
                    "[ducklink] discovery-view DDL trapped: {}: {}",
                    PREFIX_MACRO_DDL, trap
                ),
            }
        } else {
            eprintln!(
                "[ducklink] discovery-view: skipping `CREATE OR REPLACE MACRO \
                 PREFIX(...)` — the `ducklink_prefix` scalar sentinel is not \
                 yet in the core catalog (autoloaded `ducklink_scalars` / \
                 `jsonfns` artifacts missing?). `SELECT ducklink_prefix(...)` \
                 remains available and the macro will bind on the next \
                 connection after the first successful extension LOAD."
            );
        }
    }

    /// Probe the core catalog for the synthetic `ducklink_prefix` scalar
    /// (STABILITY.md § 1.1). Returns `false` if `duckdb_functions()` shows
    /// no matching entry, or if the probe itself fails (best-effort — never
    /// fatal). Used to gate the `CREATE OR REPLACE MACRO PREFIX(...)` DDL
    /// so a lean host that never triggered `drain_pending_registrations`
    /// (no autoloaded extension) doesn't surface a scary "Scalar Function
    /// does not exist" error at connection open.
    fn ducklink_prefix_scalar_registered(&self, handle: &ResourceAny) -> bool {
        const PROBE: &str =
            "SELECT 1 FROM duckdb_functions() \
             WHERE function_name = 'ducklink_prefix' \
               AND function_type = 'scalar' \
             LIMIT 1";
        let res = self.with_core(|core| {
            core.with_database(|guest, store| guest.call_execute(store, handle.clone(), PROBE))
        });
        match res {
            Ok(Ok(qr)) => !qr.rows.is_empty(),
            Ok(Err(_)) | Err(_) => false,
        }
    }

    fn with_core<F, R>(&self, f: F) -> R
    where
        F: FnOnce(&mut CoreExecution) -> R,
    {
        let mut core = self.core.lock().unwrap_or_else(|e| e.into_inner());
        f(&mut core)
    }

    /// If a prior `ducklink_load(name)` table-fn call queued an
    /// already-loaded extension for a core-side drain, replay an idempotent
    /// `LOAD <name>;` here so `ensure_extension_loaded` short-circuits and
    /// the core's post-LOAD `get_pending_registrations` picks up the
    /// manager's `deferred_registrations`. Best-effort: a trap or duckerror
    /// on the driver LOAD is logged and skipped rather than surfacing on
    /// the user's actual statement.
    ///
    /// See [`ExtensionManager::native_ducklink_load`] for why the drain has
    /// to be deferred (dispatch runs inside the core's callback path — the
    /// wasm store is mid-call and can't re-enter `call_execute`).
    fn flush_deferred_ducklink_loads(&mut self, conn: ResourceAny) {
        let names = {
            let mut manager = self
                .extension_manager
                .lock()
                .expect("extension manager mutex poisoned");
            manager.take_deferred_drain_names()
        };
        for name in names {
            // Basic identifier hygiene: `ducklink_load` already sanitizes the
            // caller's arg through `sanitize_extension_name`, but re-check
            // here — the value is embedded directly into SQL. Anything
            // non-`[A-Za-z0-9_-]` at this point would be a bug in the
            // sanitizer.
            if !name
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
            {
                eprintln!("[ducklink_load] refusing to replay LOAD for '{name}' (bad identifier)");
                continue;
            }
            let sql = format!("LOAD {name};");
            let res = self.with_core(|core| {
                core.with_database(|guest, store| guest.call_execute(store, conn.clone(), &sql))
            });
            match res {
                Ok(Ok(_)) => eprintln!(
                    "[ducklink_load] deferred drain flushed via idempotent `{sql}`"
                ),
                Ok(Err(err)) => eprintln!(
                    "[ducklink_load] deferred drain LOAD for '{name}' returned duckerror: {}",
                    core_duckerror_message(err)
                ),
                Err(trap) => eprintln!(
                    "[ducklink_load] deferred drain LOAD for '{name}' trapped: {trap}"
                ),
            }
        }
    }

    /// Drain the `(alias, namespace)` pairs the native
    /// `ducklink_prefix(...)` handler queued and, for each, run the
    /// same set of DDL statements the ducklink-extension's
    /// [`create_prefix_aliases`] + [`persist_prefix`] pair does:
    ///
    ///   1. `CREATE SCHEMA IF NOT EXISTS <alias>`
    ///   2. For every function reachable in schema `<namespace>` (per
    ///      `duckdb_functions()`, of a macro-shapeable type), emit a
    ///      `CREATE OR REPLACE MACRO <alias>.<name>(...) AS
    ///      <namespace>.<name>(...)`. See [`build_prefix_alias_macro`]
    ///      for the per-function shape.
    ///   3. `INSERT OR REPLACE INTO ducklink.prefixes(alias, namespace)`
    ///      so a subsequent host boot (or an explicit replay) can
    ///      restore the alias schema.
    ///
    /// The same rationale as [`flush_deferred_ducklink_loads`]: the
    /// handler itself runs inside a `dispatch_scalar`/`dispatch_table`
    /// callback (wasm store mid-call) and cannot re-enter
    /// `call_execute` — everything above has to happen here on the
    /// idle-core path before the user's next statement runs.
    ///
    /// Best-effort: any per-statement DDL error is logged and the pass
    /// continues, so a single bad function shape doesn't abort the
    /// user's next SQL.
    fn flush_deferred_prefix_declarations(&mut self, conn: ResourceAny) {
        let pairs = {
            let mut manager = self
                .extension_manager
                .lock()
                .expect("extension manager mutex poisoned");
            manager.take_deferred_prefix_declarations()
        };
        for (alias, namespace) in pairs {
            // Re-check hygiene at the SQL boundary: the sentinel handler
            // already ran `is_safe_prefix_identifier`, but the value is
            // spliced directly into DDL here so a defense-in-depth check
            // catches any regression that skipped the sentinel gate.
            if !is_safe_prefix_identifier(&alias) || !is_safe_prefix_identifier(&namespace) {
                eprintln!(
                    "[ducklink_prefix] refusing to flush ('{alias}', '{namespace}') \
                     — identifiers must match [A-Za-z0-9_]+"
                );
                continue;
            }
            match self.apply_prefix_declaration(conn.clone(), &alias, &namespace) {
                Ok(macros) => eprintln!(
                    "[ducklink_prefix] deferred flush: alias='{alias}' \
                     namespace='{namespace}' macros={macros}"
                ),
                Err(err) => eprintln!(
                    "[ducklink_prefix] deferred flush for ('{alias}', '{namespace}') \
                     failed: {err}"
                ),
            }
        }
    }

    /// Apply a single `(alias, namespace)` declaration: scan
    /// `duckdb_functions()` for aliasable functions in `namespace`,
    /// emit the mirrored `CREATE OR REPLACE MACRO`s under `alias`, and
    /// persist the mapping in `ducklink.prefixes`. Returns the number
    /// of macros successfully created. Mirrors the extension's
    /// `create_prefix_aliases` + `persist_prefix` pair.
    fn apply_prefix_declaration(
        &mut self,
        conn: ResourceAny,
        alias: &str,
        namespace: &str,
    ) -> Result<usize, String> {
        // 1. Ensure the alias schema exists.
        let create_schema = format!("CREATE SCHEMA IF NOT EXISTS {alias}");
        self.run_prefix_ddl(conn.clone(), &create_schema)?;

        // 2. Enumerate every aliasable function in the source namespace.
        //    (Same filter set the extension uses in `create_prefix_aliases`.)
        let scan_sql = format!(
            "SELECT DISTINCT function_name, function_type, \
                    COALESCE(array_to_string(parameters, ','), '') AS param_csv \
             FROM duckdb_functions() \
             WHERE schema_name = '{namespace}' \
             AND function_type IN ('scalar','aggregate','table_macro','scalar_macro','macro','table')"
        );
        let rows = self.run_prefix_query(conn.clone(), &scan_sql)?;
        let mut created = 0usize;
        // Dedup per (name, arity) so overloaded scalar signatures don't
        // race each other for the same LHS — matches the extension's
        // `done_arities` HashSet in `create_prefix_aliases`.
        let mut done_arities: std::collections::HashSet<(String, usize)> =
            std::collections::HashSet::new();
        for row in rows {
            let name = row.first().cloned().unwrap_or_default();
            let ftype = row.get(1).cloned().unwrap_or_default();
            let csv = row.get(2).cloned().unwrap_or_default();
            if !is_safe_prefix_identifier(&name) {
                continue;
            }
            let params: Vec<String> = if csv.is_empty() {
                Vec::new()
            } else {
                csv.split(',')
                    .map(|s| s.trim().to_string())
                    .filter(|s| !s.is_empty())
                    .collect()
            };
            if !done_arities.insert((name.clone(), params.len())) {
                continue;
            }
            let Some(macro_sql) = build_prefix_alias_macro(&ftype, alias, &name, namespace, &params)
            else {
                continue;
            };
            match self.run_prefix_ddl(conn.clone(), &macro_sql) {
                Ok(_) => created += 1,
                Err(err) => eprintln!(
                    "[ducklink_prefix] alias '{alias}.{name}' skipped: {err}"
                ),
            }
        }

        // 3. Persist the declaration so a follow-up port can replay it.
        //    Identifiers already gated above; the values themselves are
        //    string literals in SQL, so single-quotes need to be escaped.
        let alias_sql = alias.replace('\'', "''");
        let namespace_sql = namespace.replace('\'', "''");
        let insert = format!(
            "INSERT OR REPLACE INTO ducklink.prefixes (alias, namespace) \
             VALUES ('{alias_sql}', '{namespace_sql}')"
        );
        if let Err(err) = self.run_prefix_ddl(conn, &insert) {
            eprintln!(
                "[ducklink_prefix] persist ('{alias}', '{namespace}') failed: {err} \
                 (session aliases succeeded; reconnect won't restore)"
            );
        }
        Ok(created)
    }

    /// `call_execute` wrapper for one-shot DDL that returns no rows. Maps
    /// wasmtime traps + duckerrors to a single `String` for logging.
    fn run_prefix_ddl(&self, conn: ResourceAny, sql: &str) -> Result<(), String> {
        let res = self.with_core(|core| {
            core.with_database(|guest, store| guest.call_execute(store, conn, sql))
        });
        match res {
            Ok(Ok(_)) => Ok(()),
            Ok(Err(err)) => Err(core_duckerror_message(err)),
            Err(trap) => Err(format!("trap: {trap}")),
        }
    }

    /// `call_execute` wrapper for a query whose rows we need. Returns
    /// each row as `Vec<String>` (stringified via `spi_value_text`, so
    /// NULL becomes "").
    fn run_prefix_query(
        &self,
        conn: ResourceAny,
        sql: &str,
    ) -> Result<Vec<Vec<String>>, String> {
        let res = self.with_core(|core| {
            core.with_database(|guest, store| guest.call_execute(store, conn, sql))
        });
        match res {
            Ok(Ok(qr)) => Ok(qr
                .rows
                .iter()
                .map(|row| row.iter().map(spi_value_text).collect())
                .collect()),
            Ok(Err(err)) => Err(core_duckerror_message(err)),
            Err(trap) => Err(format!("trap: {trap}")),
        }
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

    // -----------------------------------------------------------------------
    // Phase 2 (@5): ATTACH intercept + write intercept helpers. See ADR
    // Decision 3 + Amendments A1 / A5 / B1 / B2.
    // -----------------------------------------------------------------------

    /// Handle a parsed `ATTACH '<dsn>' AS <alias> (TYPE <type>)` intercept.
    /// Looks up the extension registered for `<type>` in the ExtensionManager
    /// (see B1 + B2), opens the foreign catalog through the extension's
    /// `storage-dispatch.storage-attach`, and records the mapping in
    /// `attached_aliases` so subsequent SELECTs / writes route back here.
    ///
    /// Returns a synthetic empty QueryResult (like the core's own `ATTACH`
    /// return). The caller unwraps into `Ok(...)` at `execute`'s bottom.
    fn intercept_attach(
        &mut self,
        entry_handle: ResourceAny,
        spec: at5_intercept::AttachSpec,
    ) -> Result<cli_db::QueryResult, cli_types::Duckerror> {
        let attach_note = format!(
            "[at5-attach] alias={} type={} dsn={} options={:?}",
            spec.alias, spec.type_name, spec.dsn, spec.options
        );
        eprintln!("{attach_note}");
        // B1: look up the (extension, callback-handle) pair for this TYPE.
        let (ext, callback_handle) = {
            let manager = self
                .extension_manager
                .lock()
                .expect("extension manager mutex poisoned");
            match manager.storage_backend_for(&spec.type_name) {
                Some(pair) => pair,
                None => {
                    return Err(cli_types::Duckerror::Invalidargument(
                        format!(
                            "No storage extension registered for TYPE {} \
                             (attach '{}' AS {})",
                            spec.type_name, spec.dsn, spec.alias
                        )
                        .into(),
                    ));
                }
            }
        };
        if spec.if_not_exists && self.attached_aliases.contains_key(&spec.alias) {
            return Ok(empty_query_result());
        }
        // Call the extension's storage-dispatch to open the foreign catalog +
        // enumerate the tables + gather each table's schema. All under one
        // manager lock so no interleaved manipulation of the extension state
        // races us. Row descriptors are converted to the core's
        // `column-descriptor` shape for the subsequent register-table-function
        // call outside the lock.
        struct TableShape {
            name: String,
            columns: Vec<core_db_exports::ColumnDescriptor>,
            callback: u32,
        }
        let (catalog_handle, tables, table_shapes) = {
            let mut manager = self
                .extension_manager
                .lock()
                .expect("extension manager mutex poisoned");
            let catalog_handle = manager
                .dispatch_storage_attach(&spec.dsn)
                .map_err(cli_extension_duckerror)?;
            let tables = manager
                .dispatch_storage_list_tables(catalog_handle)
                .map_err(cli_extension_duckerror)?;
            let mut shapes: Vec<TableShape> = Vec::with_capacity(tables.len());
            for table in &tables {
                let cols = manager
                    .dispatch_storage_table_columns(catalog_handle, table)
                    .map_err(cli_extension_duckerror)?;
                let descriptors: Vec<core_db_exports::ColumnDescriptor> = cols
                    .into_iter()
                    .map(|c| core_db_exports::ColumnDescriptor {
                        name: c.name.into(),
                        ty: convert_extension_logicaltype_to_core(c.logical),
                    })
                    .collect();
                let handle = manager.register_at5_scan(
                    ext.clone(),
                    callback_handle,
                    catalog_handle,
                    table.clone(),
                );
                shapes.push(TableShape {
                    name: table.clone(),
                    columns: descriptors,
                    callback: handle,
                });
            }
            (catalog_handle, tables, shapes)
        };
        eprintln!(
            "[at5-attach] extension='{}' callback={} catalog={} tables={:?}",
            ext, callback_handle, catalog_handle, tables
        );

        // Materialise the read path on the core:
        //   1. Register a synthetic table function `__<alias>_<table>()` per
        //      table backed by an AT5 attach-scan callback handle.
        //   2. `ATTACH ':memory:' AS <alias>` (unless the alias already exists).
        //   3. `CREATE OR REPLACE VIEW <alias>.main.<table> AS SELECT * FROM
        //      __<alias>_<table>()` per table so `SELECT * FROM alias.table`
        //      resolves.
        // These three steps happen inside `with_core` so the mutex is briefly
        // acquired but not held across register-table-function boundaries
        // (each `guest.call_*` reads + releases in-band).
        let alias_ident = spec.alias.clone();
        for shape in &table_shapes {
            let fn_name = at5_synth_fn_name(&alias_ident, &shape.name);
            let result = self.with_core(|core| {
                core.with_database(|guest, store| {
                    guest.call_register_table_function(
                        store,
                        entry_handle.clone(),
                        &fn_name,
                        shape.columns.as_slice(),
                        shape.callback,
                    )
                })
            });
            match result {
                Ok(Ok(())) => {}
                Ok(Err(msg)) => {
                    return Err(cli_types::Duckerror::Internal(
                        format!(
                            "register_table_function for '{fn_name}' failed: {msg}"
                        )
                        .into(),
                    ));
                }
                Err(trap) => return Err(convert_trap_to_duckerror(trap)),
            }
        }

        // Only ATTACH ':memory:' on the FIRST attach for this alias; subsequent
        // re-attaches (with IF NOT EXISTS ruled out above) inherit the same
        // catalog. A second `ATTACH ':memory:' AS x` errors on the core, which
        // is the correct behaviour — duplicate alias.
        if !self.attached_aliases.contains_key(&alias_ident) {
            let attach_sql = format!("ATTACH ':memory:' AS {alias_ident}");
            let attach_res = self.with_core(|core| {
                core.with_database(|guest, store| {
                    guest.call_execute(store, entry_handle.clone(), &attach_sql)
                })
            });
            match attach_res {
                Ok(Ok(_)) => {}
                Ok(Err(err)) => return Err(convert_core_duckerror(err)),
                Err(trap) => return Err(convert_trap_to_duckerror(trap)),
            }
        }

        for shape in &table_shapes {
            let fn_name = at5_synth_fn_name(&alias_ident, &shape.name);
            let view_sql = format!(
                "CREATE OR REPLACE VIEW {alias_ident}.main.{table} AS SELECT * FROM {fn_name}()",
                alias_ident = alias_ident,
                table = shape.name,
                fn_name = fn_name,
            );
            let view_res = self.with_core(|core| {
                core.with_database(|guest, store| {
                    guest.call_execute(store, entry_handle.clone(), &view_sql)
                })
            });
            match view_res {
                Ok(Ok(_)) => {}
                Ok(Err(err)) => return Err(convert_core_duckerror(err)),
                Err(trap) => return Err(convert_trap_to_duckerror(trap)),
            }
        }

        let table_callbacks: HashMap<String, u32> = table_shapes
            .iter()
            .map(|s| (s.name.clone(), s.callback))
            .collect();
        let dsn_path = resolve_attach_dsn_path(&spec.dsn);

        // Bug 4b: probe the extension for `writes_persist_directly`. `true`
        // means the extension is authoritative for durability (sqlitewasm
        // self-persists via wasivfs, mysql/postgres write to the remote
        // server); the host skips its own serialize + fs write-back in
        // `at5_write_back`. `false` / errors keep the legacy path. Read-only
        // attaches always fall through. No host-side rusqlite connection is
        // opened; the old Bug 4 replay path was removed once sqlitewasm
        // switched to sqlite-lib.
        let writes_persist_directly = if spec.read_only {
            false
        } else {
            let mut manager = self
                .extension_manager
                .lock()
                .expect("extension manager mutex poisoned");
            let flag = matches!(
                manager.dispatch_storage_writes_persist_directly(),
                Ok(true)
            );
            if flag {
                eprintln!(
                    "[at5-writeback] {}: extension self-persists (writes_persist_directly=true); \
                     host will skip serialize+writeback",
                    spec.alias
                );
            }
            flag
        };

        self.attached_aliases.insert(
            spec.alias.clone(),
            AttachedForeignCatalog {
                extension: ext,
                catalog_handle,
                callback_handle,
                type_name: spec.type_name.clone(),
                tables,
                table_callbacks,
                dsn_path,
                read_only: spec.read_only,
                writes_persist_directly,
            },
        );
        Ok(empty_query_result())
    }

    /// Handle a parsed WRITE against an attached foreign catalog. INSERT
    /// dispatches directly to `storage_insert_rows`; UPDATE/DELETE run an
    /// Amendment A5 rowid pre-scan (`SELECT rowid, * FROM alias.table WHERE
    /// pred`) through the read path, then dispatch `storage_write_update` /
    /// `storage_write_delete` with the collected rowids (and, for UPDATE, the
    /// merged full-row payload). `WriteRoute::Unsupported` variants come from
    /// the parser (Risk 8 non-goals) and are surfaced with `Unsupported`.
    fn intercept_write(
        &mut self,
        entry_handle: ResourceAny,
        route: at5_intercept::WriteRoute,
    ) -> Result<cli_db::QueryResult, cli_types::Duckerror> {
        use at5_intercept::WriteRoute;
        match route {
            WriteRoute::Unsupported(reason) => Err(cli_types::Duckerror::Unsupported(
                format!(
                    "Operation not supported on @5 attached tables: {reason}. \
                     Use the native duckdb build if this pattern is required."
                )
                .into(),
            )),
            WriteRoute::Insert {
                alias,
                table,
                columns,
                rows,
            } => {
                let catalog = self.attached_aliases.get(&alias).ok_or_else(|| {
                    cli_types::Duckerror::Internal(
                        format!("alias {alias} disappeared before dispatch").into(),
                    )
                })?;
                let catalog_handle = catalog.catalog_handle;
                // Convert parsed literals into extension Duckvalue rows. Any
                // Raw/expression cell means we can't dispatch to the extension
                // cleanly; reject with a clear message.
                let mut ext_rows: Vec<Vec<extension_types::Duckvalue>> =
                    Vec::with_capacity(rows.len());
                for row in rows {
                    if !columns.is_empty() && row.len() != columns.len() {
                        return Err(cli_types::Duckerror::Invalidargument(
                            format!(
                                "INSERT into {alias}.{table}: {} values for {} \
                                 named columns",
                                row.len(),
                                columns.len()
                            )
                            .into(),
                        ));
                    }
                    let mut cells = Vec::with_capacity(row.len());
                    for lit in row {
                        cells.push(literal_to_extension_duckvalue(lit).map_err(|reason| {
                            cli_types::Duckerror::Unsupported(
                                format!(
                                    "Operation not supported on @5 attached tables: {reason}. \
                                     Use the native duckdb build if this pattern is required."
                                )
                                .into(),
                            )
                        })?);
                    }
                    ext_rows.push(cells);
                }
                let n = {
                    let mut manager = self
                        .extension_manager
                        .lock()
                        .expect("extension manager mutex poisoned");
                    manager
                        .dispatch_storage_insert_direct(catalog_handle, &table, &ext_rows)
                        .map_err(cli_extension_duckerror)?
                };
                eprintln!(
                    "[at5-write] INSERT {alias}.{table}: {n} row(s) dispatched to \
                     storage-write-dispatch"
                );
                // Bug 4b: no host-side replay. The extension self-persists via
                // wasivfs when it advertised writes_persist_directly at ATTACH;
                // `at5_write_back` no-ops in that case. Backends that persist
                // over the wire (mysql/postgres) also skip (serialize returns
                // Unsupported).
                self.at5_write_back(&alias, catalog_handle)?;
                Ok(empty_query_result())
            }
            WriteRoute::Update {
                alias,
                table,
                assignments,
                where_clause,
            } => {
                let (catalog_handle, columns) =
                    self.at5_lookup_write_target(&alias, &table)?;
                let rowid_idx = at5_locate_rowid_column(&columns, &alias, &table)?;
                // Parse the SET RHS values up-front so an unsupported expression
                // rejects BEFORE any core round-trip.
                let assignment_values: Vec<(String, extension_types::Duckvalue)> = assignments
                    .into_iter()
                    .map(|(col, expr)| {
                        let lit = at5_intercept::parse_value_literal(&expr);
                        literal_to_extension_duckvalue(lit)
                            .map(|v| (col, v))
                            .map_err(|reason| {
                                cli_types::Duckerror::Unsupported(
                                    format!(
                                        "Operation not supported on @5 attached tables: {reason}. \
                                         Use the native duckdb build if this pattern is required."
                                    )
                                    .into(),
                                )
                            })
                    })
                    .collect::<Result<Vec<_>, _>>()?;
                // Map each assignment column name to its index in the extension's
                // declared column list (case-insensitive).
                let mut assignment_by_idx: Vec<(usize, extension_types::Duckvalue)> =
                    Vec::with_capacity(assignment_values.len());
                for (col, val) in &assignment_values {
                    let idx = columns
                        .iter()
                        .position(|c| c.name.eq_ignore_ascii_case(col))
                        .ok_or_else(|| {
                            cli_types::Duckerror::Invalidargument(
                                format!(
                                    "UPDATE {alias}.{table}: column '{col}' is not \
                                     declared by the storage extension"
                                )
                                .into(),
                            )
                        })?;
                    assignment_by_idx.push((idx, val.clone()));
                }
                // Pre-scan the entire row so we can preserve non-SET columns
                // verbatim. `SELECT * FROM alias.main.table [WHERE pred]`
                // resolves against the view registered by intercept_attach,
                // which routes through the extension's storage-dispatch.
                let (rowids, current_rows) =
                    self.at5_prescan_rows(entry_handle.clone(), &alias, &table, rowid_idx, where_clause.as_deref())?;
                if rowids.is_empty() {
                    eprintln!(
                        "[at5-write] UPDATE {alias}.{table}: pre-scan matched 0 row(s); no-op"
                    );
                    return Ok(empty_query_result());
                }
                // Merge assignment values into each current row.
                let updated_rows: Vec<Vec<extension_types::Duckvalue>> = current_rows
                    .into_iter()
                    .map(|mut row| {
                        for (idx, val) in &assignment_by_idx {
                            if let Some(cell) = row.get_mut(*idx) {
                                *cell = val.clone();
                            }
                        }
                        row
                    })
                    .collect();
                let n = {
                    let mut manager = self
                        .extension_manager
                        .lock()
                        .expect("extension manager mutex poisoned");
                    manager
                        .dispatch_storage_update_direct(
                            catalog_handle,
                            &table,
                            &rowids,
                            &updated_rows,
                        )
                        .map_err(cli_extension_duckerror)?
                };
                eprintln!(
                    "[at5-write] UPDATE {alias}.{table}: {n} row(s) dispatched to \
                     storage-write-dispatch (rowids={rowids:?})"
                );
                // Bug 4b: no host-side replay — see the INSERT arm's comment.
                self.at5_write_back(&alias, catalog_handle)?;
                Ok(empty_query_result())
            }
            WriteRoute::CreateTable {
                alias,
                table,
                columns,
                if_not_exists,
            } => {
                let catalog = self.attached_aliases.get(&alias).ok_or_else(|| {
                    cli_types::Duckerror::Internal(
                        format!("alias {alias} disappeared before dispatch").into(),
                    )
                })?;
                let catalog_handle = catalog.catalog_handle;
                // Bail out cleanly for IF NOT EXISTS when the table is already
                // there. The extension's `storage_list_tables` reflects the
                // remote schema, so we don't need to teach the extension a
                // separate "exists" probe.
                if if_not_exists {
                    let existing = {
                        let mut manager = self
                            .extension_manager
                            .lock()
                            .expect("extension manager mutex poisoned");
                        manager
                            .dispatch_storage_list_tables(catalog_handle)
                            .map_err(cli_extension_duckerror)?
                    };
                    if existing.iter().any(|t| t.eq_ignore_ascii_case(&table)) {
                        eprintln!(
                            "[at5-write] CREATE TABLE IF NOT EXISTS {alias}.{table}: \
                             table already exists; no-op"
                        );
                        return Ok(empty_query_result());
                    }
                }
                let mut ext_cols: Vec<extension_types::Columndef> =
                    Vec::with_capacity(columns.len());
                for col in &columns {
                    let name = &col.name;
                    let type_text = &col.type_name;
                    let logical = sql_type_to_extension_logical(type_text).map_err(|reason| {
                        cli_types::Duckerror::Unsupported(
                            format!(
                                "Operation not supported on @5 attached tables: \
                                 CREATE TABLE {alias}.{table}: column '{name}' type \
                                 '{type_text}' -- {reason}. Use the native duckdb \
                                 build if this pattern is required."
                            )
                            .into(),
                        )
                    })?;
                    ext_cols.push(extension_types::Columndef {
                        name: name.clone(),
                        logical,
                    });
                }
                {
                    let mut manager = self
                        .extension_manager
                        .lock()
                        .expect("extension manager mutex poisoned");
                    manager
                        .dispatch_storage_create_table_direct(catalog_handle, &table, &ext_cols)
                        .map_err(cli_extension_duckerror)?
                }
                eprintln!(
                    "[at5-write] CREATE TABLE {alias}.{table}: {} column(s) dispatched \
                     to storage-write-dispatch",
                    ext_cols.len()
                );
                // The catalog snapshot refresh after intercept_write returns
                // (see the call site in call_execute) will pick up the new
                // table via a re-listing on the next scan.
                self.at5_write_back(&alias, catalog_handle)?;
                Ok(empty_query_result())
            }
            WriteRoute::Delete {
                alias,
                table,
                where_clause,
            } => {
                let (catalog_handle, columns) =
                    self.at5_lookup_write_target(&alias, &table)?;
                let rowid_idx = at5_locate_rowid_column(&columns, &alias, &table)?;
                let (rowids, _rows) =
                    self.at5_prescan_rows(entry_handle.clone(), &alias, &table, rowid_idx, where_clause.as_deref())?;
                if rowids.is_empty() {
                    eprintln!(
                        "[at5-write] DELETE {alias}.{table}: pre-scan matched 0 row(s); no-op"
                    );
                    return Ok(empty_query_result());
                }
                let n = {
                    let mut manager = self
                        .extension_manager
                        .lock()
                        .expect("extension manager mutex poisoned");
                    manager
                        .dispatch_storage_delete_direct(catalog_handle, &table, &rowids)
                        .map_err(cli_extension_duckerror)?
                };
                eprintln!(
                    "[at5-write] DELETE {alias}.{table}: {n} row(s) dispatched to \
                     storage-write-dispatch (rowids={rowids:?})"
                );
                // Bug 4b: no host-side replay — see the INSERT arm's comment.
                self.at5_write_back(&alias, catalog_handle)?;
                Ok(empty_query_result())
            }
            WriteRoute::CreateTable {
                alias,
                table,
                columns,
                if_not_exists,
            } => {
                let catalog = self.attached_aliases.get(&alias).ok_or_else(|| {
                    cli_types::Duckerror::Internal(
                        format!("alias {alias} disappeared before dispatch").into(),
                    )
                })?;
                let catalog_handle = catalog.catalog_handle;
                // Convert the parsed (name, type_name-string) pairs into the
                // extension's Columndef shape. The extension is responsible
                // for mapping its own logical types back to native column
                // types on the wire (see e.g. postgreswasm's
                // `logical_to_postgres`).
                let ext_columns: Vec<extension_types::Columndef> = columns
                    .iter()
                    .map(|c| extension_types::Columndef {
                        name: c.name.clone(),
                        logical: parse_sql_type_to_logical(&c.type_name),
                    })
                    .collect();
                let dispatch = {
                    let mut manager = self
                        .extension_manager
                        .lock()
                        .expect("extension manager mutex poisoned");
                    manager.dispatch_storage_create_table_direct(
                        catalog_handle,
                        &table,
                        &ext_columns,
                    )
                };
                match dispatch {
                    Ok(()) => {
                        eprintln!(
                            "[at5-write] CREATE TABLE {alias}.{table}: dispatched to \
                             storage-write-dispatch ({} cols)",
                            ext_columns.len()
                        );
                    }
                    Err(err) => {
                        // With IF NOT EXISTS, tolerate a duplicate-table
                        // error (each backend spells it differently — match
                        // on message text). Anything else propagates.
                        if if_not_exists {
                            let msg = extension_duckerror_message(&err).to_ascii_lowercase();
                            if msg.contains("already exists") || msg.contains("duplicate") {
                                eprintln!(
                                    "[at5-write] CREATE TABLE IF NOT EXISTS {alias}.{table}: \
                                     table already present; skipping"
                                );
                                return Ok(empty_query_result());
                            }
                        }
                        return Err(cli_extension_duckerror(err));
                    }
                }
                self.at5_write_back(&alias, catalog_handle)?;
                Ok(empty_query_result())
            }
        }
    }

    /// AT5 write-back: called after every successful INSERT/UPDATE/DELETE
    /// dispatch on an attached foreign catalog. Serializes the extension's
    /// in-memory representation via `storage-dispatch.serialize` and writes
    /// the resulting blob back to the ATTACH DSN file so the mutation persists
    /// on disk.
    ///
    /// Skips silently when:
    ///  - the alias vanished (race with DETACH, defensive),
    ///  - the ATTACH used a non-file DSN (connection string, remote URI) — no
    ///    local path to write to,
    ///  - the ATTACH was READ_ONLY (belt-and-braces; the parser already
    ///    rejects the write, but a future codepath might not),
    ///  - the extension returns `Duckerror::Unsupported` (its backend isn't a
    ///    serializable blob — the mutation already round-tripped over the wire
    ///    via storage-write-dispatch).
    ///
    /// Other extension errors (I/O, internal) propagate as CLI Duckerrors so
    /// the write is reported as failed even though the in-memory copy was
    /// mutated. Callers must treat this as fatal.
    fn at5_write_back(
        &mut self,
        alias: &str,
        catalog_handle: u32,
    ) -> Result<(), cli_types::Duckerror> {
        let (dsn_path, read_only, self_persists) = match self.attached_aliases.get(alias) {
            Some(c) => (c.dsn_path.clone(), c.read_only, c.writes_persist_directly),
            None => return Ok(()),
        };
        if read_only {
            return Ok(());
        }
        // Bug 4b: the extension advertised `writes_persist_directly = true`
        // at ATTACH time (sqlitewasm self-persists via wasivfs into a host
        // preopen; mysql/postgres write over the wire). Nothing to write back
        // from the host — the DSN file / remote server is already up to date.
        // Skip the O(N) full-serialize + rewrite entirely.
        if self_persists {
            return Ok(());
        }
        let Some(dsn_path) = dsn_path else {
            // DSN wasn't a local file — nothing to write back to.
            return Ok(());
        };
        let bytes = {
            let mut manager = self
                .extension_manager
                .lock()
                .expect("extension manager mutex poisoned");
            match manager.dispatch_storage_serialize(catalog_handle) {
                Ok(bytes) => bytes,
                Err(extension_types::Duckerror::Unsupported(_)) => {
                    eprintln!(
                        "[at5-writeback] {alias}: serialize Unsupported; \
                         skipping fs write (backend is not a serializable blob)"
                    );
                    return Ok(());
                }
                Err(err) => return Err(cli_extension_duckerror(err)),
            }
        };
        std::fs::write(&dsn_path, &bytes).map_err(|e| {
            cli_types::Duckerror::Io(
                format!(
                    "AT5 write-back to {}: {e}",
                    dsn_path.display()
                )
                .into(),
            )
        })?;
        eprintln!(
            "[at5-writeback] {alias}: wrote {} byte(s) back to {}",
            bytes.len(),
            dsn_path.display()
        );
        Ok(())
    }

    // Bug 4b: the at5_file_conn / at5_replay_{insert,update,delete} helpers
    // that lived here (~150 LOC) were dropped when sqlitewasm moved to
    // sqlite-lib + wasivfs. The extension self-persists inside the wasm
    // sandbox, so the host has no serialize-and-rewrite work to do and no
    // native rusqlite dependency. See `at5_write_back`'s self_persists
    // short-circuit.


    /// Fetch (catalog-handle, extension-declared columns) for an attached
    /// alias.table pair, briefly acquiring the manager lock. Errors are
    /// mapped to CLI Duckerror so the caller can `?` them.
    fn at5_lookup_write_target(
        &mut self,
        alias: &str,
        table: &str,
    ) -> Result<(u32, Vec<extension_types::Columndef>), cli_types::Duckerror> {
        let catalog_handle = self
            .attached_aliases
            .get(alias)
            .map(|c| c.catalog_handle)
            .ok_or_else(|| {
                cli_types::Duckerror::Internal(
                    format!("alias {alias} disappeared before dispatch").into(),
                )
            })?;
        let cols = {
            let mut manager = self
                .extension_manager
                .lock()
                .expect("extension manager mutex poisoned");
            manager
                .dispatch_storage_table_columns(catalog_handle, table)
                .map_err(cli_extension_duckerror)?
        };
        Ok((catalog_handle, cols))
    }

    /// Amendment A5 pre-scan: execute `SELECT * FROM alias.main.table [WHERE
    /// pred]` through the read path (which routes through the intercept_attach
    /// view + synthetic table function), returning parallel `(rowids, rows)`
    /// vectors. `rowid_idx` is the position of the rowid column in the
    /// extension's declared column list; the returned rows retain the full
    /// column set (rowid included) so UPDATE can splice SET values into any
    /// non-rowid position without touching rowid itself.
    fn at5_prescan_rows(
        &mut self,
        entry_handle: ResourceAny,
        alias: &str,
        table: &str,
        rowid_idx: usize,
        where_clause: Option<&str>,
    ) -> Result<(Vec<i64>, Vec<Vec<extension_types::Duckvalue>>), cli_types::Duckerror> {
        let where_sql = where_clause
            .map(|p| format!(" WHERE {p}"))
            .unwrap_or_default();
        let sql = format!("SELECT * FROM {alias}.main.{table}{where_sql}");
        let result = self
            .with_core(|core| {
                core.with_database(|guest, store| {
                    guest.call_execute(store, entry_handle, &sql)
                })
            })
            .map_err(convert_trap_to_duckerror)?;
        let qr = match result {
            Ok(v) => v,
            Err(err) => return Err(convert_core_duckerror(err)),
        };
        let mut rowids: Vec<i64> = Vec::with_capacity(qr.rows.len());
        let mut rows_ext: Vec<Vec<extension_types::Duckvalue>> =
            Vec::with_capacity(qr.rows.len());
        for row in qr.rows.into_iter() {
            let row_vec: Vec<core_types::Duckvalue> = row.into_iter().collect();
            let rowid_cell = row_vec.get(rowid_idx).ok_or_else(|| {
                cli_types::Duckerror::Internal(
                    format!(
                        "pre-scan row for {alias}.{table} lacks rowid column at \
                         index {rowid_idx} (row has {} cells)",
                        row_vec.len()
                    )
                    .into(),
                )
            })?;
            let rowid = at5_duckvalue_to_i64(rowid_cell).map_err(|reason| {
                cli_types::Duckerror::Internal(
                    format!(
                        "pre-scan row for {alias}.{table}: rowid column is not \
                         an integer ({reason})"
                    )
                    .into(),
                )
            })?;
            rowids.push(rowid);
            rows_ext.push(
                row_vec
                    .into_iter()
                    .map(convert_core_duckvalue_to_extension)
                    .collect(),
            );
        }
        Ok((rowids, rows_ext))
    }
}

fn empty_query_result() -> cli_db::QueryResult {
    cli_db::QueryResult {
        columns: Vec::new().into(),
        rows: Vec::new().into(),
    }
}

/// Phase 2c: synthetic table-function name for the ATTACH intercept's
/// per-table shim. `__<alias>_<table>` — clean identifier chars only;
/// callers must have already validated alias + table via the AT5 parser
/// (`at5_intercept::is_bare_ident`), so this is a pure concat.
fn at5_synth_fn_name(alias: &str, table: &str) -> String {
    format!("__{alias}_{table}")
}

// Bug 4b: `sqlite_quote_ident`, `sqlite_err_io`, `duck_to_sqlite_value`
// (formerly ~50 LOC of Duckvalue -> rusqlite::Value type-adapter) were
// dropped when the host-side rusqlite replay path was removed. The
// sqlitewasm extension now runs the equivalent conversion inside the wasm
// sandbox against sqlite-lib's SPI value type (see
// extensions/sqlitewasm-component/src/lib.rs::duck_to_sqlite_value).


/// AT5 write-back path resolver. Turns the raw ATTACH DSN into a local file
/// path when the DSN plausibly names a local file: bare paths, and the
/// `file://` URL form. Returns None for connection strings, remote URIs, or
/// paths that don't exist (so a remote-only backend never triggers a
/// write-back fs write). The path is not required to exist at attach time —
/// callers may create it — but this best-effort resolution mirrors the same
/// "is this a file dsn?" test `dispatch_storage_attach` uses when staging
/// blob bytes: if there's no reasonable local path, there's no write-back.
fn resolve_attach_dsn_path(dsn: &str) -> Option<PathBuf> {
    let raw = dsn.trim();
    if raw.is_empty() {
        return None;
    }
    // file:// URL form: strip the scheme, leaving whatever the URL held
    // (typically an absolute path). Percent-decoding is not attempted — every
    // caller today (the AT5 tests, the smoke suite) hands us plain filesystem
    // paths that don't need it, so implementing decode here would be dead
    // code the first bug is likely to expose.
    let path_str = if let Some(rest) = raw.strip_prefix("file://") {
        // Two-slash `file:` (with an empty authority) collapses to `/…`; the
        // three-slash `file:///…` form drops the authority entirely and keeps
        // the leading `/`. Both land here as `rest` starting with `/`.
        rest.to_string()
    } else if raw.contains("://") {
        // Any other URI scheme (http, s3, mysql, postgres, ...) is not a
        // local file — no write-back for these.
        return None;
    } else {
        raw.to_string()
    };
    Some(PathBuf::from(path_str))
}

/// Amendment A5: find the extension-declared `rowid` column (case-insensitive)
/// in a storage-dispatch column list. Missing = the extension doesn't expose
/// a stable row identifier; UPDATE/DELETE can't run against it.
fn at5_locate_rowid_column(
    columns: &[extension_types::Columndef],
    alias: &str,
    table: &str,
) -> Result<usize, cli_types::Duckerror> {
    columns
        .iter()
        .position(|c| c.name.eq_ignore_ascii_case("rowid"))
        .ok_or_else(|| {
            cli_types::Duckerror::Unsupported(
                format!(
                    "UPDATE/DELETE against {alias}.{table} requires the storage \
                     extension to expose a 'rowid' column (ADR Amendment A5); \
                     the extension declared columns [{}]",
                    columns
                        .iter()
                        .map(|c| c.name.as_str())
                        .collect::<Vec<_>>()
                        .join(", ")
                )
                .into(),
            )
        })
}

/// Amendment A5: coerce a `Duckvalue` returned by the pre-scan's rowid column
/// into a plain i64. Accepts every signed / unsigned integer arm; other arms
/// are rejected (returning a reason string the caller wraps in a Duckerror).
fn at5_duckvalue_to_i64(v: &core_types::Duckvalue) -> Result<i64, String> {
    Ok(match v {
        core_types::Duckvalue::Int64(n) => *n,
        core_types::Duckvalue::Int32(n) => *n as i64,
        core_types::Duckvalue::Int16(n) => *n as i64,
        core_types::Duckvalue::Int8(n) => *n as i64,
        core_types::Duckvalue::Uint64(n) => {
            i64::try_from(*n).map_err(|_| format!("u64 rowid {n} overflows i64"))?
        }
        core_types::Duckvalue::Uint32(n) => *n as i64,
        core_types::Duckvalue::Uint16(n) => *n as i64,
        core_types::Duckvalue::Uint8(n) => *n as i64,
        other => return Err(format!("rowid duckvalue arm {other:?} is not an integer")),
    })
}


fn cli_extension_duckerror(err: extension_types::Duckerror) -> cli_types::Duckerror {
    match err {
        extension_types::Duckerror::Invalidargument(m) => {
            cli_types::Duckerror::Invalidargument(m.into())
        }
        extension_types::Duckerror::Unsupported(m) => cli_types::Duckerror::Unsupported(m.into()),
        extension_types::Duckerror::Invalidstate(m) => cli_types::Duckerror::Invalidstate(m.into()),
        extension_types::Duckerror::Io(m) => cli_types::Duckerror::Io(m.into()),
        extension_types::Duckerror::Internal(m) => cli_types::Duckerror::Internal(m.into()),
    }
}

/// Extract the human-readable text out of an extension `Duckerror`. Used by
/// the write intercept's `CREATE TABLE IF NOT EXISTS` arm to detect the
/// backend-specific "table already exists" wording without cracking every
/// variant open individually.
fn extension_duckerror_message(err: &extension_types::Duckerror) -> &str {
    match err {
        extension_types::Duckerror::Invalidargument(m)
        | extension_types::Duckerror::Unsupported(m)
        | extension_types::Duckerror::Invalidstate(m)
        | extension_types::Duckerror::Io(m)
        | extension_types::Duckerror::Internal(m) => m.as_str(),
    }
}

/// Map a raw SQL type token from a `CREATE TABLE alias.table (col TYPE, ...)`
/// intercept into an extension `Logicaltype`. Only the common SQL type spellings
/// used by the AT5 test suite (INTEGER / BIGINT / SMALLINT / TEXT / VARCHAR /
/// BOOL / REAL / DOUBLE / BYTEA / BLOB) are mapped here — everything else is
/// forwarded as `Complex` so an extension that supports it can decide.
/// Case-insensitive; strips a trailing width/scale specifier
/// (`VARCHAR(255)` -> `VARCHAR`) before matching.
fn parse_sql_type_to_logical(ty: &str) -> extension_types::Logicaltype {
    let mut t = ty.trim();
    if let Some(paren) = t.find('(') {
        t = t[..paren].trim();
    }
    let up = t.to_ascii_uppercase();
    match up.as_str() {
        "INT" | "INTEGER" | "INT4" => extension_types::Logicaltype::Int32,
        "BIGINT" | "INT8" => extension_types::Logicaltype::Int64,
        "SMALLINT" | "INT2" => extension_types::Logicaltype::Int16,
        "TINYINT" => extension_types::Logicaltype::Int8,
        "UBIGINT" => extension_types::Logicaltype::Uint64,
        "UINTEGER" => extension_types::Logicaltype::Uint32,
        "USMALLINT" => extension_types::Logicaltype::Uint16,
        "UTINYINT" => extension_types::Logicaltype::Uint8,
        "REAL" | "FLOAT4" => extension_types::Logicaltype::Float32,
        "DOUBLE" | "FLOAT" | "FLOAT8" | "DOUBLE PRECISION" => {
            extension_types::Logicaltype::Float64
        }
        "BOOL" | "BOOLEAN" => extension_types::Logicaltype::Boolean,
        "TEXT" | "STRING" | "VARCHAR" | "CHAR" => extension_types::Logicaltype::Text,
        "BLOB" | "BYTEA" | "BYTES" => extension_types::Logicaltype::Blob,
        "DATE" => extension_types::Logicaltype::Date,
        "TIME" => extension_types::Logicaltype::Time,
        "TIMESTAMP" | "DATETIME" => extension_types::Logicaltype::Timestamp,
        "TIMESTAMPTZ" => extension_types::Logicaltype::Timestamptz,
        "UUID" => extension_types::Logicaltype::Uuid,
        _ => extension_types::Logicaltype::Complex(t.to_string()),
    }
}

fn literal_to_extension_duckvalue(
    lit: at5_intercept::ValueLiteral,
) -> Result<extension_types::Duckvalue, String> {
    use at5_intercept::ValueLiteral;
    Ok(match lit {
        ValueLiteral::Null => extension_types::Duckvalue::Null,
        ValueLiteral::Integer(n) => extension_types::Duckvalue::Int64(n),
        ValueLiteral::Float(f) => extension_types::Duckvalue::Float64(f),
        ValueLiteral::String(s) => extension_types::Duckvalue::Text(s.into()),
        ValueLiteral::Blob(b) => extension_types::Duckvalue::Blob(b.into()),
        ValueLiteral::Raw(expr) => {
            return Err(format!(
                "unsupported expression `{expr}` in VALUES tuple (only literals \
                 NULL/int/float/'string'/X'hex' are routed to the extension)"
            ));
        }
    })
}

/// Map a raw SQL column-type text (e.g. `INTEGER`, `TEXT`, `VARCHAR(20)`,
/// `DECIMAL(10,2)`, `DOUBLE PRECISION`) to the extension's `Logicaltype`.
/// Only the leading TYPE tokens are considered — column constraints (PRIMARY
/// KEY / NOT NULL / DEFAULT ...) must be stripped by the caller. Length /
/// width / scale specifiers are accepted and mostly ignored (the exception
/// is DECIMAL, which parses `(width,scale)` into the DECIMAL arm).
///
/// Used by `HostState::intercept_write`'s `CreateTable` branch to convert
/// parsed columns into `extension_types::Columndef` before dispatching
/// through `storage-write-dispatch.create-table`. Returns `Err(reason)` for
/// unrecognized types so the caller can surface a clean `Unsupported` error.
fn sql_type_to_extension_logical(
    type_text: &str,
) -> Result<extension_types::Logicaltype, String> {
    let s = type_text.trim();
    if s.is_empty() {
        return Err("empty type text".to_string());
    }
    // Peel off the trailing `(...)` specifier so `VARCHAR(20)` matches
    // `VARCHAR`. DECIMAL is handled specially below (needs width+scale).
    let (base, spec) = match s.find('(') {
        Some(open) => {
            let close = s.rfind(')').ok_or_else(|| {
                format!("unbalanced parentheses in type '{s}'")
            })?;
            if close < open {
                return Err(format!("mismatched parentheses in type '{s}'"));
            }
            let base = s[..open].trim();
            let inside = s[open + 1..close].trim();
            let tail = s[close + 1..].trim();
            let full_base = if tail.is_empty() {
                base.to_string()
            } else {
                // e.g. `DOUBLE PRECISION` — keep both words when the
                // specifier is empty; won't hit this arm since `DOUBLE
                // PRECISION` has no `(...)`.
                format!("{base} {tail}")
            };
            (full_base, Some(inside.to_string()))
        }
        None => (s.to_string(), None),
    };
    let up: String = base.split_whitespace().collect::<Vec<_>>().join(" ").to_ascii_uppercase();
    Ok(match up.as_str() {
        "BOOLEAN" | "BOOL" => extension_types::Logicaltype::Boolean,
        "TINYINT" | "INT1" => extension_types::Logicaltype::Int8,
        "SMALLINT" | "INT2" => extension_types::Logicaltype::Int16,
        "INT" | "INTEGER" | "INT4" => extension_types::Logicaltype::Int32,
        "BIGINT" | "INT8" | "INT64" => extension_types::Logicaltype::Int64,
        "HUGEINT" | "INT128" => extension_types::Logicaltype::Hugeint,
        "UTINYINT" | "UINT1" => extension_types::Logicaltype::Uint8,
        "USMALLINT" | "UINT2" => extension_types::Logicaltype::Uint16,
        "UINTEGER" | "UINT4" => extension_types::Logicaltype::Uint32,
        "UBIGINT" | "UINT8" => extension_types::Logicaltype::Uint64,
        "UHUGEINT" | "UINT128" => extension_types::Logicaltype::Uhugeint,
        "REAL" | "FLOAT" | "FLOAT4" => extension_types::Logicaltype::Float32,
        "DOUBLE" | "DOUBLE PRECISION" | "FLOAT8" => extension_types::Logicaltype::Float64,
        "TEXT" | "VARCHAR" | "CHAR" | "CHARACTER" | "STRING" | "CLOB" => {
            extension_types::Logicaltype::Text
        }
        "BLOB" | "BYTEA" | "BYTES" | "BINARY" | "VARBINARY" => {
            extension_types::Logicaltype::Blob
        }
        "DATE" => extension_types::Logicaltype::Date,
        "TIME" => extension_types::Logicaltype::Time,
        "TIMESTAMP" | "DATETIME" => extension_types::Logicaltype::Timestamp,
        "TIMESTAMPTZ" | "TIMESTAMP_TZ" | "TIMESTAMP WITH TIME ZONE" => {
            extension_types::Logicaltype::Timestamptz
        }
        "INTERVAL" => extension_types::Logicaltype::Interval,
        "UUID" => extension_types::Logicaltype::Uuid,
        "DECIMAL" | "NUMERIC" => {
            let (width, scale) = match spec.as_deref() {
                Some(inside) => {
                    let parts: Vec<&str> = inside.split(',').map(|p| p.trim()).collect();
                    match parts.as_slice() {
                        [w] => (
                            w.parse::<u8>().map_err(|_| {
                                format!("DECIMAL width '{w}' is not a small integer")
                            })?,
                            0u8,
                        ),
                        [w, sc] => (
                            w.parse::<u8>().map_err(|_| {
                                format!("DECIMAL width '{w}' is not a small integer")
                            })?,
                            sc.parse::<u8>().map_err(|_| {
                                format!("DECIMAL scale '{sc}' is not a small integer")
                            })?,
                        ),
                        _ => return Err(format!("DECIMAL takes 1 or 2 args, got '{inside}'")),
                    }
                }
                None => (18, 3), // DuckDB's default DECIMAL when unspecified.
            };
            extension_types::Logicaltype::Decimal(extension_types::Decimalshape { width, scale })
        }
        other => {
            return Err(format!(
                "unrecognized SQL type '{other}' (add a mapping in \
                 sql_type_to_extension_logical if the storage backend supports it)"
            ));
        }
    })
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

    /// @5.0.0: The CLI shell never serves quack RPC through its connection;
    /// the quack extension's bridge server (if started) is driven directly from
    /// the core-side host bindings (see `handle_quack_request` in core lib.rs).
    fn handle_quack_request(
        &mut self,
        _body: wasmtime::component::__internal::Vec<u8>,
    ) -> Option<wasmtime::component::__internal::Vec<u8>> {
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
                // nested-exec Direction-1 §5.(b.1): remember which DB the primary
                // just opened so a later extension `nested_exec` can materialize
                // the sibling core against the same file. `None` = in-memory,
                // which the sibling cannot share -> nested_exec errors clearly.
                if let Some(sibling) = self.sibling.as_ref() {
                    sibling.record_primary_open(sanitize_sibling_open_path(owned.as_deref()));
                }
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
                // nested-exec Direction-1 §5.(b.1): record the primary's opened
                // path — see the mirror call in `open` above.
                if let Some(sibling) = self.sibling.as_ref() {
                    sibling.record_primary_open(sanitize_sibling_open_path(owned_path.as_deref()));
                }
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
        let entry_handle = self
            .connections
            .get(&conn.rep())
            .ok_or_else(|| cli_types::Duckerror::Internal("unknown connection".into()))?
            .handle
            .clone();
        // Phase 2 (@5) multi-statement AT5 intercept: `-c "LOAD ext; ATTACH …
        // (TYPE ext); SELECT * FROM alias.tbl;"` reaches us as a SINGLE
        // execute() call (the wasm CLI's `run_script` accumulates lines until
        // a `;`, so a one-line multi-statement payload is dispatched as one
        // blob). `parse_attach` / `resolve_write_target` only recognize a
        // single statement, so a mid-batch ATTACH would fall through to the
        // core and be rejected as "Unrecognized storage type <name>". Split
        // top-level statements HERE (respecting quotes + comments) and
        // dispatch each through `execute_single_statement` — the intercept
        // path runs against each statement in turn, and non-intercept
        // statements pass through to the core exactly as before. Only the
        // LAST statement's result is returned (matches DuckDB's own
        // multi-statement-script semantics).
        let sql_ref: &str = sql.as_ref();
        if at5_intercept::is_multi_statement(sql_ref) {
            let pieces: Vec<String> = at5_intercept::split_statements(sql_ref)
                .into_iter()
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty())
                .collect();
            let mut last: cli_db::QueryResult = empty_query_result();
            for piece in pieces {
                last = self.execute_single_statement(entry_handle.clone(), &piece)?;
            }
            return Ok(last);
        }
        self.execute_single_statement(entry_handle, sql_ref)
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

    /// Phase 2c: bridge a CLI-side `database.register-table-function` call
    /// through to the core's own `database.register-table-function` export
    /// (see ADR Amendment A1 + Phase 2b Part A). The CLI's imported view
    /// carries a distinct-but-structurally-identical `Connection` /
    /// `ColumnDescriptor` shape from the core's export view, so this trait
    /// impl resolves the CLI connection resource to the core-side connection
    /// handle, converts each column descriptor CLI -> core, and forwards.
    ///
    /// Phase 2c's `intercept_attach` calls the core directly (via
    /// `with_core` + `call_register_table_function`) rather than routing
    /// through this trait — but if a CLI-facing component ever wants to
    /// register a host-owned table function, this stub is now live.
    fn register_table_function(
        &mut self,
        conn: Resource<cli_db::Connection>,
        name: CliString,
        columns: wasmtime::component::__internal::Vec<cli_db::ColumnDescriptor>,
        callback_handle: u32,
    ) -> Result<(), CliString> {
        let entry_handle = self
            .connections
            .get(&conn.rep())
            .ok_or_else(|| {
                CliString::from("register_table_function: unknown connection resource")
            })?
            .handle
            .clone();
        let core_name: String = name.into();
        let core_columns: Vec<core_db_exports::ColumnDescriptor> = columns
            .into_iter()
            .map(convert_cli_columndescriptor_to_core)
            .collect();
        let result = self
            .with_core(|core| {
                core.with_database(|guest, store| {
                    guest.call_register_table_function(
                        store,
                        entry_handle,
                        &core_name,
                        core_columns.as_slice(),
                        callback_handle,
                    )
                })
            })
            .map_err(trap_to_cli_string)?;
        match result {
            Ok(()) => Ok(()),
            Err(err) => Err(err.into()),
        }
    }
}

impl HostState {
    /// One-statement execute-body: intercept ATTACH / write-against-attached
    /// first, then fall through to the core's `call_execute`. Extracted so
    /// the multi-statement dispatch in `cli_db::Host::execute` above can
    /// loop over pre-split statements without recursing through the WIT
    /// `Resource<Connection>` boundary (which is not `Clone`).
    fn execute_single_statement(
        &mut self,
        entry_handle: ResourceAny,
        sql: &str,
    ) -> Result<cli_db::QueryResult, cli_types::Duckerror> {
        // `ducklink_load(name)` deferred drain: if the user's PREVIOUS
        // statement went through `dispatch_table`'s native `ducklink_load`
        // path, an extension was loaded but its pending registrations are
        // stashed in the manager (that path can't re-enter `call_execute`
        // from a wasm-store-mid-call callback). Replay an idempotent
        // `LOAD <name>;` here — `ensure_extension_loaded` short-circuits
        // (already in `self.extensions`) and the core then calls
        // `get_pending_registrations`, which drains our `deferred_registrations`
        // into the DuckDB catalog. The user's actual SQL runs after.
        self.flush_deferred_ducklink_loads(entry_handle.clone());
        // Same deferral rationale as `ducklink_load`: the native
        // `ducklink_prefix(alias, namespace)` sentinel handler validates
        // + queues the pair but can't run the associated `CREATE SCHEMA
        // / CREATE OR REPLACE MACRO / INSERT INTO ducklink.prefixes` DDL
        // in-band (mid-callback re-entry into `call_execute` deadlocks
        // the core mutex). Drain the queue here — the core is idle again
        // — so `<alias>.<fn>` resolves for the user's next statement.
        self.flush_deferred_prefix_declarations(entry_handle.clone());
        // Phase 2 (@5): ATTACH + write intercept for foreign catalogs backed
        // by a storage-capable extension. Both branches run BEFORE the SQL
        // reaches the core (see ADR Decision 3 + Amendment A1 + A5).
        //
        // ATTACH: parse `ATTACH '<dsn>' AS <alias> (TYPE <name> [, k=v ...])`,
        // route to the extension's storage-dispatch, materialize the foreign
        // catalog into an in-memory attached DB on the core, and record the
        // alias in `attached_aliases` so subsequent writes are routed here too.
        // Falls through to the core for any ATTACH shape we don't recognize.
        if let Some(spec) = at5_intercept::parse_attach(sql) {
            match self.intercept_attach(entry_handle.clone(), spec) {
                Ok(result) => {
                    self.refresh_catalog_snapshot();
                    return Ok(result);
                }
                Err(err) => return Err(err),
            }
        }
        // WRITE: parse INSERT/UPDATE/DELETE against an attached alias. See
        // `resolve_write_target` for the accept/reject matrix (Amendment A2
        // Risk 8 non-goals return `Unsupported` here).
        if !self.attached_aliases.is_empty() {
            let alias_map: HashMap<String, String> =
                self.attached_aliases.keys().map(|k| (k.clone(), String::new())).collect();
            if let Some(route) = at5_intercept::resolve_write_target(sql, &alias_map) {
                match self.intercept_write(entry_handle.clone(), route) {
                    Ok(result) => {
                        self.refresh_catalog_snapshot();
                        return Ok(result);
                    }
                    Err(err) => return Err(err),
                }
            }
        }
        // One-query `delta_scan('dir')`: the wasm core can't take a subquery-
        // valued table-fn arg, so the host reads the table's _delta_log off the
        // real filesystem, resolves the active files (add minus remove), and
        // rewrites the call to a read_parquet([...]) the core can scan. No-op
        // when the SQL has no rewritable delta_scan call.
        let sql = delta_rewrite::rewrite_delta_scan(sql, &self.preopens);
        let result = self
            .with_core(|core| {
                // Option (a) nested-exec (docs/nested-exec-direction-1-plan.md
                // §7.8): snapshot raw pointers to the primary core so a
                // scalar/table callback firing inside this call_execute can
                // re-enter the SAME store via
                // `PrimaryReentryGuard`/`primary_nested_exec` instead of the
                // shipped (b.1) sibling. RAII restores TLS on any path
                // (Ok, Err, panic), so a nested_exec never sees a stale
                // pointer to a freed CoreExecution.
                //
                // Borrow-checker note: the raw-pointer coercions (`&mut
                // core.store as *mut _`, `&core.bindings as *const _`) drop
                // their borrows at the end of the coercion expression, so
                // the subsequent `core.with_database(...)` &mut re-borrow
                // is unaliased.
                let store_ptr: *mut Store<CoreStoreState> = &mut core.store;
                let bindings_ptr: *const duckdb_core_bindings::Libduckdb =
                    &core.bindings;
                let _reentry = PrimaryReentryGuard::set(PrimaryReentry {
                    store: store_ptr,
                    bindings: bindings_ptr,
                    connection: entry_handle,
                });
                core.with_database(|guest, store| {
                    guest.call_execute(store, entry_handle, &sql)
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

/// Phase 2c: CLI-side `column-descriptor` -> core-side `column-descriptor`.
/// The two bindgen expansions generate structurally-identical types (both
/// vendor the same `duckdb:component/database.column-descriptor` WIT record),
/// but wasmtime treats them as distinct nominal types so we must convert
/// explicitly. Used by `HostState::register_table_function` and by
/// `intercept_attach`.
fn convert_cli_columndescriptor_to_core(
    col: cli_db::ColumnDescriptor,
) -> core_db_exports::ColumnDescriptor {
    core_db_exports::ColumnDescriptor {
        name: col.name.into(),
        ty: convert_cli_logicaltype_to_core(col.ty),
    }
}

/// Phase 2c: inverse of `convert_core_logicaltype`. Both `cli_types` and
/// `core_types` re-export `duckdb:extension/types.logicaltype`; the arm list
/// must stay in sync with the WIT (see `convert_core_logicaltype` above for
/// the reverse direction).
fn convert_cli_logicaltype_to_core(ty: cli_types::Logicaltype) -> core_types::Logicaltype {
    match ty {
        cli_types::Logicaltype::Boolean => core_types::Logicaltype::Boolean,
        cli_types::Logicaltype::Int64 => core_types::Logicaltype::Int64,
        cli_types::Logicaltype::Uint64 => core_types::Logicaltype::Uint64,
        cli_types::Logicaltype::Float64 => core_types::Logicaltype::Float64,
        cli_types::Logicaltype::Text => core_types::Logicaltype::Text,
        cli_types::Logicaltype::Blob => core_types::Logicaltype::Blob,
        cli_types::Logicaltype::Int32 => core_types::Logicaltype::Int32,
        cli_types::Logicaltype::Timestamp => core_types::Logicaltype::Timestamp,
        cli_types::Logicaltype::Int8 => core_types::Logicaltype::Int8,
        cli_types::Logicaltype::Int16 => core_types::Logicaltype::Int16,
        cli_types::Logicaltype::Uint8 => core_types::Logicaltype::Uint8,
        cli_types::Logicaltype::Uint16 => core_types::Logicaltype::Uint16,
        cli_types::Logicaltype::Uint32 => core_types::Logicaltype::Uint32,
        cli_types::Logicaltype::Float32 => core_types::Logicaltype::Float32,
        cli_types::Logicaltype::Date => core_types::Logicaltype::Date,
        cli_types::Logicaltype::Time => core_types::Logicaltype::Time,
        cli_types::Logicaltype::Timestamptz => core_types::Logicaltype::Timestamptz,
        cli_types::Logicaltype::Decimal(shape) => {
            core_types::Logicaltype::Decimal(core_types::Decimalshape {
                width: shape.width,
                scale: shape.scale,
            })
        }
        cli_types::Logicaltype::Interval => core_types::Logicaltype::Interval,
        cli_types::Logicaltype::Uuid => core_types::Logicaltype::Uuid,
        cli_types::Logicaltype::Hugeint => core_types::Logicaltype::Hugeint,
        cli_types::Logicaltype::Uhugeint => core_types::Logicaltype::Uhugeint,
        cli_types::Logicaltype::Complex(expr) => core_types::Logicaltype::Complex(expr),
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
        // @5.0.0: DECIMAL carries a decimalshape { width, scale } payload
        // structurally on the variant arm. `core_runtime_exports` re-exports
        // the shared type from core_types.
        reg::LogicalType::Decimal { width, scale } => {
            core_runtime_exports::Logicaltype::Decimal(core_types::Decimalshape {
                width,
                scale,
            })
        }
        reg::LogicalType::Interval => core_runtime_exports::Logicaltype::Interval,
        reg::LogicalType::Uuid => core_runtime_exports::Logicaltype::Uuid,
        // @5.0.0: first-class fieldless 128-bit integer logical types.
        reg::LogicalType::Hugeint => core_runtime_exports::Logicaltype::Hugeint,
        reg::LogicalType::UHugeint => core_runtime_exports::Logicaltype::Uhugeint,
        // S1 (major-5): nested logical types (LIST / STRUCT / MAP / ARRAY)
        // added on the neutral side ride out as type-expr strings through
        // core's Complex arm — the core WIT has no nested shape.
        reg::LogicalType::List(elem) => core_runtime_exports::Logicaltype::Complex(format!(
            "LIST({})",
            neutral_logicaltype_to_type_expr(&elem)
        )),
        reg::LogicalType::Struct(fields) => {
            let mut acc = String::from("STRUCT(");
            for (i, (n, t)) in fields.iter().enumerate() {
                if i > 0 {
                    acc.push_str(", ");
                }
                acc.push_str(n);
                acc.push(' ');
                acc.push_str(&neutral_logicaltype_to_type_expr(t));
            }
            acc.push(')');
            core_runtime_exports::Logicaltype::Complex(acc)
        }
        reg::LogicalType::Map(k, v) => core_runtime_exports::Logicaltype::Complex(format!(
            "MAP({}, {})",
            neutral_logicaltype_to_type_expr(&k),
            neutral_logicaltype_to_type_expr(&v)
        )),
        reg::LogicalType::Array(size, elem) => core_runtime_exports::Logicaltype::Complex(format!(
            "{}[{}]",
            neutral_logicaltype_to_type_expr(&elem),
            size
        )),
        reg::LogicalType::Complex(expr) => core_runtime_exports::Logicaltype::Complex(expr),
    }
}

/// Best-effort rendering of a neutral `reg::LogicalType` as a DuckDB SQL type
/// expression. Used by the core down-cast when the target (core @4.0.0) has no
/// structural place for a v5 nested / hugeint arm and must fall back to
/// `Complex(type-expr)`.
fn neutral_logicaltype_to_type_expr(ty: &reg::LogicalType) -> String {
    match ty {
        reg::LogicalType::Boolean => "BOOLEAN".into(),
        reg::LogicalType::Int64 => "BIGINT".into(),
        reg::LogicalType::Uint64 => "UBIGINT".into(),
        reg::LogicalType::Float64 => "DOUBLE".into(),
        reg::LogicalType::Text => "VARCHAR".into(),
        reg::LogicalType::Blob => "BLOB".into(),
        reg::LogicalType::Int32 => "INTEGER".into(),
        reg::LogicalType::Timestamp => "TIMESTAMP".into(),
        reg::LogicalType::Int8 => "TINYINT".into(),
        reg::LogicalType::Int16 => "SMALLINT".into(),
        reg::LogicalType::Uint8 => "UTINYINT".into(),
        reg::LogicalType::Uint16 => "USMALLINT".into(),
        reg::LogicalType::Uint32 => "UINTEGER".into(),
        reg::LogicalType::Float32 => "FLOAT".into(),
        reg::LogicalType::Date => "DATE".into(),
        reg::LogicalType::Time => "TIME".into(),
        reg::LogicalType::Timestamptz => "TIMESTAMPTZ".into(),
        reg::LogicalType::Decimal { width, scale } => format!("DECIMAL({width}, {scale})"),
        reg::LogicalType::Interval => "INTERVAL".into(),
        reg::LogicalType::Uuid => "UUID".into(),
        reg::LogicalType::Hugeint => "HUGEINT".into(),
        reg::LogicalType::UHugeint => "UHUGEINT".into(),
        reg::LogicalType::List(elem) => format!("{}[]", neutral_logicaltype_to_type_expr(elem)),
        reg::LogicalType::Struct(fields) => {
            let mut acc = String::from("STRUCT(");
            for (i, (n, t)) in fields.iter().enumerate() {
                if i > 0 {
                    acc.push_str(", ");
                }
                acc.push_str(n);
                acc.push(' ');
                acc.push_str(&neutral_logicaltype_to_type_expr(t));
            }
            acc.push(')');
            acc
        }
        reg::LogicalType::Map(k, v) => format!(
            "MAP({}, {})",
            neutral_logicaltype_to_type_expr(k),
            neutral_logicaltype_to_type_expr(v)
        ),
        reg::LogicalType::Array(size, elem) => {
            format!("{}[{}]", neutral_logicaltype_to_type_expr(elem), size)
        }
        reg::LogicalType::Complex(expr) => expr.clone(),
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

// 3.1.0 additive minor: a neutral `reg::LogicalType` -> the core `types`
// Logicaltype (used by the host-import `table-stream-host.filterable-table` shape,
// whose columndef carries `types.logicaltype`).
fn neutral_reg_logicaltype_to_core_types(ty: reg::LogicalType) -> core_types::Logicaltype {
    use core_types::Logicaltype as C;
    match ty {
        reg::LogicalType::Boolean => C::Boolean,
        reg::LogicalType::Int64 => C::Int64,
        reg::LogicalType::Uint64 => C::Uint64,
        reg::LogicalType::Float64 => C::Float64,
        reg::LogicalType::Text => C::Text,
        reg::LogicalType::Blob => C::Blob,
        reg::LogicalType::Int32 => C::Int32,
        reg::LogicalType::Timestamp => C::Timestamp,
        reg::LogicalType::Int8 => C::Int8,
        reg::LogicalType::Int16 => C::Int16,
        reg::LogicalType::Uint8 => C::Uint8,
        reg::LogicalType::Uint16 => C::Uint16,
        reg::LogicalType::Uint32 => C::Uint32,
        reg::LogicalType::Float32 => C::Float32,
        reg::LogicalType::Date => C::Date,
        reg::LogicalType::Time => C::Time,
        reg::LogicalType::Timestamptz => C::Timestamptz,
        // @5.0.0: DECIMAL carries width/scale on the variant arm.
        reg::LogicalType::Decimal { width, scale } => {
            C::Decimal(core_types::Decimalshape { width, scale })
        }
        reg::LogicalType::Interval => C::Interval,
        reg::LogicalType::Uuid => C::Uuid,
        // @5.0.0: first-class fieldless HUGEINT / UHUGEINT.
        reg::LogicalType::Hugeint => C::Hugeint,
        reg::LogicalType::UHugeint => C::Uhugeint,
        // S1 (major-5): nested types ride out as type-expr strings.
        reg::LogicalType::List(elem) => {
            C::Complex(format!("LIST({})", neutral_logicaltype_to_type_expr(&elem)))
        }
        reg::LogicalType::Struct(fields) => {
            let mut acc = String::from("STRUCT(");
            for (i, (n, t)) in fields.iter().enumerate() {
                if i > 0 {
                    acc.push_str(", ");
                }
                acc.push_str(n);
                acc.push(' ');
                acc.push_str(&neutral_logicaltype_to_type_expr(t));
            }
            acc.push(')');
            C::Complex(acc)
        }
        reg::LogicalType::Map(k, v) => C::Complex(format!(
            "MAP({}, {})",
            neutral_logicaltype_to_type_expr(&k),
            neutral_logicaltype_to_type_expr(&v)
        )),
        reg::LogicalType::Array(size, elem) => C::Complex(format!(
            "{}[{}]",
            neutral_logicaltype_to_type_expr(&elem),
            size
        )),
        reg::LogicalType::Complex(expr) => C::Complex(expr),
    }
}

// Phase 2 (@5): the core no longer imports `table-stream-host`, so there is
// no ts-filter clause crossing from the core to translate. The extension-side
// table-filter shape stays intact; the host builds those directly from its
// intercepted plan (see ATTACH intercept in HostState::execute).

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
        // @5.0.0: first-class 128-bit integer arms carry (lower, upper) halves.
        core_types::Duckvalue::Hugeint(h) => cli_types::Duckvalue::Hugeint(cli_types::Hugeintvalue {
            lower: h.lower,
            upper: h.upper,
        }),
        core_types::Duckvalue::Uhugeint(h) => cli_types::Duckvalue::Uhugeint(cli_types::Uhugeintvalue {
            lower: h.lower,
            upper: h.upper,
        }),
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
        // @5.0.0: first-class 128-bit integer arms carry (lower, upper) halves.
        cli_types::Duckvalue::Hugeint(h) => core_types::Duckvalue::Hugeint(core_types::Hugeintvalue {
            lower: h.lower,
            upper: h.upper,
        }),
        cli_types::Duckvalue::Uhugeint(h) => core_types::Duckvalue::Uhugeint(core_types::Uhugeintvalue {
            lower: h.lower,
            upper: h.upper,
        }),
        cli_types::Duckvalue::Complex(c) => {
            core_types::Duckvalue::Complex(core_types::Complexvalue {
                type_expr: c.type_expr,
                json: c.json,
            })
        }
        // T2-1 residual: CLI wit is @5 (Hugeint/Uhugeint have first-class
        // arms) while core is still @4 (only the Complex escape hatch).
        // Serialize the 128-bit value as base-10 text and label it with
        // the type-expr — same shape convert_core_extension_duckvalue uses.
        cli_types::Duckvalue::Hugeint(h) => {
            let v: i128 = ((h.upper as i128) << 64) | (h.lower as i128);
            core_types::Duckvalue::Complex(core_types::Complexvalue {
                type_expr: "HUGEINT".into(),
                json: v.to_string(),
            })
        }
        cli_types::Duckvalue::Uhugeint(h) => {
            let v: u128 = ((h.upper as u128) << 64) | (h.lower as u128);
            core_types::Duckvalue::Complex(core_types::Complexvalue {
                type_expr: "UHUGEINT".into(),
                json: v.to_string(),
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
        // @5.0.0: decimal now carries a decimalshape { width, scale } payload.
        core_types::Logicaltype::Decimal(shape) => {
            cli_types::Logicaltype::Decimal(cli_types::Decimalshape {
                width: shape.width,
                scale: shape.scale,
            })
        }
        core_types::Logicaltype::Interval => cli_types::Logicaltype::Interval,
        core_types::Logicaltype::Uuid => cli_types::Logicaltype::Uuid,
        // @5.0.0: first-class 128-bit integer logical types (fieldless).
        core_types::Logicaltype::Hugeint => cli_types::Logicaltype::Hugeint,
        core_types::Logicaltype::Uhugeint => cli_types::Logicaltype::Uhugeint,
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
    // nested-exec Direction-1 §5.(b.1): shared sibling-core state, lazily
    // materialized on first `nested_exec` call. `None` when the host did not
    // wire a sibling (narrow test paths); `nested_exec` then reports
    // unavailability instead of trapping.
    sibling: Option<Arc<SiblingState>>,
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

    // Direction-1 nested-exec (§5.(b.1) of `docs/nested-exec-direction-1-plan.md`):
    // route `sql` to a SIBLING [`CoreExecution`] opened against the same DB file
    // as the primary. The sibling lives in its own wasmtime store + its own
    // `ExtensionManager`, so it never re-enters the primary core's store or its
    // (contended) mutex from inside an outer statement's callback.
    //
    // Sibling init is LAZY: the first call after `HostState::open` opens a
    // fresh core over the primary's DB path and caches it; subsequent calls
    // reuse it for the process lifetime. The [`NestedExecDepthGuard`] applied
    // by `extension_nested_exec::Host::nested_exec` in ducklink-runtime bounds
    // recursion; nothing to do here.
    //
    // KNOWN LIMITATION. The sibling has NONE of the primary's extensions
    // loaded, so SQL that references an extension-provided function fails with
    // `Catalog Error: ... does not exist`. That failure shape is detected via
    // [`is_extension_related_error`] and the error is wrapped with
    // [`NESTED_EXEC_DIRECTION2_REDIRECT`] pointing the caller at Direction 2
    // (native ducklink extension). Non-extension errors pass through verbatim.
    //
    // 2026-07-25 (option (a), §7.8): if we were called from a scalar/table
    // callback that fired inside an outer `HostState::execute`,
    // `PRIMARY_STORE_REENTRY` is set — dispatch directly on the PRIMARY
    // store + the outer connection instead of on the sibling. Writes are
    // then visible to the outer statement's continuation + the outer
    // connection's catalog (fixes the fieldbook two-catalog bug §7.4).
    // The sibling path below stays as a fallback for callers reached
    // without the TLS set (narrow tests, or a future non-callback caller).
    fn nested_exec(&mut self, sql: &str) -> Result<NestedExecResult, String> {
        if let Some(reentry) = PRIMARY_STORE_REENTRY.with(|slot| slot.get()) {
            // SAFETY: preconditions documented on `primary_nested_exec` +
            // `PrimaryReentryGuard`. The RAII guard installed by
            // `HostState::execute` guarantees the pointers name a live
            // `CoreExecution` on the outer stack frame, and this call is
            // synchronous on the same thread.
            return unsafe { primary_nested_exec(reentry, sql) };
        }

        let sibling = self
            .sibling
            .as_ref()
            .ok_or_else(|| {
                "nested-exec: sibling-core state not wired in this host \
                 (Direction-1 §5.(b.1) support requires SiblingState via \
                 ExtensionManager::attach_sibling_state)"
                    .to_string()
            })?
            .clone();

        // Resolve the primary's opened DB path. `open` writes it via
        // `SiblingState::record_primary_open`; :memory: is rejected because two
        // in-memory opens are independent Database objects (no sharing).
        let primary_path = {
            let guard = sibling
                .primary_db_path
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            match guard.as_ref() {
                Some(Some(p)) => p.clone(),
                Some(None) => {
                    return Err(
                        "nested-exec: primary database is in-memory; \
                         Direction-1 sibling-core cannot share it. \
                         Open a file-backed database or use the native \
                         ducklink DuckDB extension (Direction 2)."
                            .to_string(),
                    )
                }
                None => {
                    return Err(
                        "nested-exec: primary database not yet opened \
                         (Direction-1 sibling waits for HostState::open)"
                            .to_string(),
                    )
                }
            }
        };

        // Lazy-init the sibling core + connection (once per process).
        let slot = sibling_ensure_slot(&sibling, &primary_path)?;

        // Run the SQL on the sibling connection. Its mutex is independent of
        // the primary's, and we're always the outermost frame from the
        // sibling's perspective — no try_lock gymnastics needed.
        let mut core = slot.core.lock().unwrap_or_else(|e| e.into_inner());
        let outcome = core
            .with_database(|guest, store| guest.call_execute(store, slot.connection, sql))
            .map_err(|trap| format!("nested-exec: sibling call_execute trapped: {trap}"))?;
        drop(core);

        match outcome {
            Ok(qr) => Ok(query_result_to_nested_exec(qr)),
            Err(err) => {
                let msg = core_duckerror_message(err);
                if is_extension_related_error(&msg) {
                    Err(format!("{NESTED_EXEC_DIRECTION2_REDIRECT}{msg}"))
                } else {
                    Err(msg)
                }
            }
        }
    }
}

/// Option (a) nested-exec: dispatch `sql` on the PRIMARY core store +
/// the outer CLI connection using the raw pointers snapshotted by
/// [`PrimaryReentryGuard`] in `HostState::execute`. The write lands on
/// the primary connection, so the outer statement's catalog + any
/// subsequent CLI statement see it immediately.
///
/// # Safety
///
/// * `reentry.store` must name a live `Store<CoreStoreState>` whose
///   `&mut` borrow is logically held by an outer `HostState::execute`
///   frame on the same thread — set by [`PrimaryReentryGuard::set`] and
///   cleared on drop, so a stale slot is impossible.
/// * `reentry.bindings` must point to the [`duckdb_core_bindings::Libduckdb`]
///   attached to the same `CoreExecution`.
/// * `reentry.connection` must be a live [`ResourceAny`] in that store's
///   resource table (the CLI's entry connection recorded by
///   `HostState::execute`).
///
/// wasmtime tolerates the re-entrant `call_execute` (verified by
/// `tests/reentrancy_poc.rs::wall2_wasmtime_permits_reentry_from_host_callback_with_caller`).
/// The Rust `&mut Store<T>` fabricated from the raw pointer aliases the
/// outer stack frame's `&mut StoreInner<T>` for the duration of this
/// call, but wasmtime's internal store state is designed for this
/// pattern (Caller/StoreContextMut is the safe surface of the same
/// primitive; here we go around wit-bindgen's data-only adapter to reach
/// it).
unsafe fn primary_nested_exec(
    reentry: PrimaryReentry,
    sql: &str,
) -> Result<NestedExecResult, String> {
    let bindings = unsafe { &*reentry.bindings };
    let store: &mut Store<CoreStoreState> = unsafe { &mut *reentry.store };
    let guest = bindings.duckdb_component_database();
    let outcome = guest
        .call_execute(store.as_context_mut(), reentry.connection, sql)
        .map_err(|trap| format!("nested-exec: primary call_execute trapped: {trap}"))?;
    match outcome {
        Ok(qr) => Ok(query_result_to_nested_exec(qr)),
        Err(err) => Err(core_duckerror_message(err)),
    }
}

/// Materialize (once) the sibling [`CoreExecution`] + its [`ResourceAny`]
/// connection over `primary_path`, cache them on `sibling`, and return a
/// clone of the cached [`SiblingSlot`]. Idempotent: after the first success
/// every call returns the cached slot instantly.
fn sibling_ensure_slot(
    sibling: &SiblingState,
    primary_path: &str,
) -> Result<SiblingSlot, String> {
    let mut slot = sibling.slot.lock().unwrap_or_else(|e| e.into_inner());
    if let Some(existing) = slot.as_ref() {
        return Ok(SiblingSlot {
            core: existing.core.clone(),
            connection: existing.connection,
        });
    }

    // Build the sibling's WASI ctx with the SAME preopen set the primary
    // received. The primary resolves user-facing paths (e.g.
    // `open "/data/foo.duckdb"`) against a preopen mapping; the sibling must
    // resolve them identically to reach the same file. Stdio is inherited so
    // DuckDB's diagnostics still reach the terminal.
    let preopen_refs: Vec<(&Path, &str)> = sibling
        .preopens
        .iter()
        .map(|(host, guest)| (host.as_path(), guest.as_str()))
        .collect();
    let wasi = build_wasi_ctx_inherit(&[String::from("duckdb-core-sibling")], &preopen_refs)
        .map_err(|e| format!("nested-exec: sibling WASI ctx: {e}"))?;
    // Phase 4: if the harness wired the primary's `ExtensionManager` onto this
    // `SiblingState`, the sibling core's `CoreStoreState` binds to that SHARED
    // manager — its `callback-dispatch` host import then routes to the
    // primary's loaded [`ExtensionInstance`] map (via the shared
    // `callback_registry`), so `nested-exec` SQL can reach extension-provided
    // functions. Fall back to a fresh manager for narrow test paths that
    // never opt in (preserves the historical §5.(b.1) fresh-sibling
    // behaviour + the [`NESTED_EXEC_DIRECTION2_REDIRECT`] error shape).
    //
    // NOTE: when the SHARED path is taken, we intentionally do NOT call
    // `attach_core(sibling_core)` on the manager — that would overwrite the
    // primary's `self.core` reference (used by `ensure_extension_loaded` to
    // build extension `CoreServices`). The sibling's own core reference is
    // held on `SiblingSlot` below; the shared manager's `self.core` keeps
    // pointing at the primary as intended.
    let (sibling_manager, is_shared) = match sibling.extension_manager.as_ref() {
        Some(shared) => (shared.clone(), true),
        None => (
            Arc::new(Mutex::new(ExtensionManager::new(sibling.engine.clone()))),
            false,
        ),
    };
    let mut core_exec = instantiate_core(
        &sibling.engine,
        &sibling.core_component_path,
        wasi,
        sibling_manager.clone(),
    )
    .map_err(|e| format!("nested-exec: sibling instantiate_core: {e}"))?;
    // Phase 4 follow-up (FU4): wire this sibling store to the shared replay
    // archive so its post-LOAD `get_pending_registrations` returns the
    // primary's accumulated registrations instead of an empty drain. Wiring
    // even in the `!is_shared` fallback keeps `is_sibling == true` at the
    // dispatch layer — the fresh manager's drain buffer will just be empty
    // for that path (backward-compatible: no regressions in the fresh-manager
    // tests, which don't LOAD any extension on the sibling).
    core_exec.attach_replay_archive(sibling.replay_archive.clone(), true);
    let core = Arc::new(Mutex::new(core_exec));
    if !is_shared {
        // Only wire the sibling's own core reference onto its OWN fresh
        // manager. When sharing, the manager's `core` field already names the
        // primary — do not clobber it (see NOTE above).
        let mut mgr = sibling_manager
            .lock()
            .expect("sibling extension manager mutex poisoned");
        mgr.attach_core(core.clone());
    }
    // Open the sibling's connection to the SAME DB file the primary opened.
    let connection = {
        let mut c = core.lock().unwrap_or_else(|e| e.into_inner());
        let result = c
            .with_database(|guest, store| guest.call_open(store, Some(primary_path)))
            .map_err(|trap| format!("nested-exec: sibling call_open trapped: {trap}"))?;
        result.map_err(|e| format!("nested-exec: sibling open failed: {e}"))?
    };

    // Phase 4 follow-up (FU4): replay the primary's extension registrations
    // onto the sibling's DuckDB catalog. A single `LOAD <name>` on the
    // sibling triggers its wasm shim's `get_pending_registrations` host
    // import, which now returns the archive above — one call registers every
    // scalar / table / aggregate / macro / cast / replacement-scan the
    // primary drained, in one shot, via the shim's normal C-API path. Any
    // subsequent extension name in the list is skipped: a second LOAD would
    // ask for the archive again and DuckDB would reject the re-registration.
    //
    // Best-effort:
    //   * an empty `loaded_extensions` list (no extensions loaded yet on the
    //     primary) → skip, sibling can still serve pure-DDL/DML nested_exec.
    //   * a LOAD trap / duckerror is logged and skipped — the sibling's
    //     bootstrap SQL takes precedence over the replay convenience.
    let replay_name = sibling
        .loaded_extensions
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .first()
        .cloned();
    if let Some(name) = replay_name {
        // Basic identifier hygiene (matches `flush_deferred_ducklink_loads`).
        if name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
        {
            let sql = format!("LOAD {name};");
            let mut c = core.lock().unwrap_or_else(|e| e.into_inner());
            let outcome = c.with_database(|guest, store| guest.call_execute(store, connection, &sql));
            drop(c);
            match outcome {
                Ok(Ok(_)) => eprintln!(
                    "[sibling-replay] LOAD {name} on sibling primed \
                     get_pending_registrations from the shared archive"
                ),
                Ok(Err(err)) => eprintln!(
                    "[sibling-replay] LOAD {name} on sibling returned duckerror: {} \
                     (registration replay may be partial)",
                    core_duckerror_message(err)
                ),
                Err(trap) => eprintln!(
                    "[sibling-replay] LOAD {name} on sibling trapped: {trap} \
                     (registration replay skipped)"
                ),
            }
        } else {
            eprintln!(
                "[sibling-replay] refusing to LOAD '{name}' on sibling (bad identifier)"
            );
        }
    }

    *slot = Some(SiblingSlot {
        core: core.clone(),
        connection,
    });
    Ok(SiblingSlot { core, connection })
}

/// Stringify a wasm-core `QueryResult` into the neutral [`NestedExecResult`]
/// shape (rows of text cells, mirroring Direction-2's row rendering). Always
/// populates `rows`; also populates `rows_affected` from the single-column
/// `Count` scalar DuckDB emits for pure DML.
fn query_result_to_nested_exec(qr: core_db_exports::QueryResult) -> NestedExecResult {
    // DuckDB reports DML (INSERT/UPDATE/DELETE with no RETURNING) as a
    // single-column result named "Count" carrying the affected row count.
    // Extract it BEFORE stringifying so the caller sees rows_affected without
    // parsing the string cell back out.
    let rows_affected = extract_rows_affected(&qr);
    let rows: Vec<Vec<String>> = qr
        .rows
        .iter()
        .map(|row| row.iter().map(spi_value_text).collect())
        .collect();
    NestedExecResult {
        rows: Some(rows),
        rows_affected,
    }
}

/// Detect DuckDB's pure-DML pattern (single row, single column named `Count`
/// holding an integer) and return the affected-row count. Everything else
/// returns `None` — the caller relies on `rows` alone for SELECT and mixed
/// (RETURNING) shapes.
fn extract_rows_affected(qr: &core_db_exports::QueryResult) -> Option<u64> {
    if qr.columns.len() != 1 {
        return None;
    }
    if !qr.columns[0].name.eq_ignore_ascii_case("Count") {
        return None;
    }
    let row = qr.rows.first()?;
    let cell = row.first()?;
    match cell {
        core_types::Duckvalue::Int64(v) => Some((*v).max(0) as u64),
        core_types::Duckvalue::Uint64(v) => Some(*v),
        core_types::Duckvalue::Int32(v) => Some((*v).max(0) as u64),
        core_types::Duckvalue::Uint32(v) => Some(*v as u64),
        _ => None,
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
    add_wasi_http_to_linker(&mut linker)?;
    core_host_loader::add_to_linker::<CoreStoreState, CoreStoreState>(&mut linker, |state| state)?;
    core_extension_hooks::add_to_linker::<CoreStoreState, CoreStoreState>(&mut linker, |state| {
        state
    })?;
    core_callback_dispatch::add_to_linker::<CoreStoreState, CoreStoreState>(
        &mut linker,
        |state| state,
    )?;
    // Phase 2 (@5): the 8 `*-host` linker registrations
    // (storage / index / collation / pragma / parser / optimizer / files /
    // table-stream) are DELETED. Those imports no longer exist on the core
    // world -- their capabilities lift to the host's SQL-level ATTACH intercept
    // and write intercept (see HostState::execute). See ADR Decision 3.
    core_tvm_manager::add_to_linker::<CoreStoreState, CoreStoreState>(&mut linker, |state| state)?;
    core_tvm_bytes::add_to_linker::<CoreStoreState, CoreStoreState>(&mut linker, |state| state)?;

    let mut store = Store::new(
        engine,
        CoreStoreState {
            table: ResourceTable::new(),
            wasi: wasi_ctx,
            wasi_http: WasiHttpCtx::new(),
            extension_manager,
            tvm: tvm_core::RegionDirectory::new(),
            tvm_slots: std::collections::HashMap::new(),
            // Phase 4 follow-up (FU4): defaults — a primary caller wires the
            // shared archive via `CoreExecution::attach_replay_archive(archive,
            // false)` once its `SiblingState` exists; `sibling_ensure_slot`
            // does the same with `is_sibling = true` on the sibling store.
            // Test paths that never wire a SiblingState keep both defaults,
            // reproducing the pre-FU4 behaviour (plain drain, no archive).
            replay_archive: None,
            is_sibling: false,
        },
    );

    let instance_pre = linker.instantiate_pre(&component)?;
    let pre = duckdb_core_bindings::LibduckdbPre::new(instance_pre)?;
    let bindings = pre.instantiate(store.as_context_mut())?;
    Ok(CoreExecution { store, bindings })
}

/// Trust gate for precompiled `.cwasm` files.
///
/// A `.cwasm` deserializes to runnable native code, so loading one is
/// `unsafe`: a tampered or foreign-engine file would otherwise be executed.
/// We authenticate every `.cwasm` with `compose-core`'s [`CompileCache`]
/// (HMAC-SHA256 over a per-machine secret, bound to the engine identity), the
/// single shared trust model also used by sqlink's CAS. The HMAC key lives in
/// `~/.cache/ducklink/compile-hmac.key` (0600); if it can't be loaded we warn
/// once and fall back to today's unauthenticated behavior so caching never
/// hard-fails a run.
mod cwasm_trust {
    use super::*;
    use compose_core::blobs::compute_digest;
    use compose_core::{CompileCache, FsBlobStore};
    use std::sync::OnceLock;

    /// Engine identity: anything that invalidates a `.cwasm` on upgrade. Bound
    /// into the HMAC key so a wasmtime/host bump can never open an old frame.
    fn engine_version() -> String {
        format!(
            "ducklink-host-{}-wasmtime-{}-cm-exn",
            env!("CARGO_PKG_VERSION"),
            wasmtime_version()
        )
    }

    fn wasmtime_version() -> &'static str {
        // The pinned wasmtime version from Cargo (matches Cargo.toml).
        "46.0.1"
    }

    fn target() -> String {
        format!("{}-{}", std::env::consts::ARCH, std::env::consts::OS)
    }

    /// Per-machine HMAC secret; created once at 0600 if absent. `None` ->
    /// degrade to unauthenticated load (warn once).
    fn hmac_key() -> Option<Vec<u8>> {
        static KEY: OnceLock<Option<Vec<u8>>> = OnceLock::new();
        KEY.get_or_init(load_or_create_key).clone()
    }

    fn key_path() -> Option<std::path::PathBuf> {
        dirs::cache_dir().map(|d| d.join("ducklink").join("compile-hmac.key"))
    }

    fn load_or_create_key() -> Option<Vec<u8>> {
        let path = key_path()?;
        if let Ok(bytes) = std::fs::read(&path) {
            if bytes.len() == 32 {
                return Some(bytes);
            }
        }
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).ok()?;
        }
        let mut key = [0u8; 32];
        getrandom_fill(&mut key)?;
        if std::fs::write(&path, &key).is_err() {
            warn_once("failed to persist compile-cache HMAC key");
            return None;
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600));
        }
        Some(key.to_vec())
    }

    fn getrandom_fill(buf: &mut [u8]) -> Option<()> {
        // /dev/urandom is available everywhere the host runs; avoids pulling a
        // new rng crate into the host.
        std::fs::read(std::path::Path::new("/dev/urandom"))
            .ok()
            .and_then(|pool| {
                if pool.len() >= buf.len() {
                    buf.copy_from_slice(&pool[..buf.len()]);
                    Some(())
                } else {
                    None
                }
            })
    }

    fn warn_once(msg: &str) {
        static WARNED: OnceLock<()> = OnceLock::new();
        if WARNED.set(()).is_ok() {
            eprintln!("warning: {msg}; falling back to unauthenticated .cwasm load");
        }
    }

    /// A [`CompileCache`] over a throwaway in-memory-ish FS root (only its
    /// HMAC primitives are used for the file-path `.cwasm`; the blob backend
    /// is never queried). `None` when no key is available.
    fn cache() -> Option<CompileCache<FsBlobStore>> {
        let key = hmac_key()?;
        // FsBlobStore needs a root, but seal/open never touch it; point it at
        // the cache dir so the type is satisfied without extra I/O.
        let root = key_path()?.parent()?.join("blobs");
        let backend = FsBlobStore::new(root, u64::MAX).ok()?;
        Some(CompileCache::new(backend, key))
    }

    /// Seal precompiled bytes for `wasm_bytes` into the on-disk `.cwasm` frame
    /// (`hmac_tag || precompiled`). Falls back to raw bytes if no key.
    pub fn seal_cwasm(wasm_bytes: &[u8], precompiled: &[u8]) -> Vec<u8> {
        match cache() {
            Some(c) => {
                let digest = compute_digest(wasm_bytes);
                c.seal(&digest, &engine_version(), &target(), precompiled)
            }
            None => {
                warn_once("no compile-cache HMAC key");
                precompiled.to_vec()
            }
        }
    }

    /// Open a `.cwasm` frame, returning the authenticated precompiled bytes.
    /// Returns `None` if the frame fails HMAC verification AND a key exists
    /// (tamper/foreign engine); returns the raw bytes when no key is available
    /// (degraded mode) or when the frame is unsealed legacy content.
    pub fn open_cwasm(framed: &[u8]) -> Option<Vec<u8>> {
        // We cannot recompute the source wasm digest from the .cwasm alone, so
        // we verify against the engine identity using a wildcard component
        // digest: the HMAC binds (component_digest, engine, target). To keep
        // the file self-describing we prepend the 32-byte source-wasm digest to
        // the frame in `write_sealed_cwasm`; parse it here.
        let (digest, frame) = framed.split_at_checked(32)?;
        match cache() {
            Some(c) => {
                match c.open(&digest.to_vec(), &engine_version(), &target(), frame) {
                    Some(bytes) => Some(bytes),
                    None => {
                        warn_once(".cwasm failed HMAC verification (tamper or engine mismatch)");
                        None
                    }
                }
            }
            // No key: degraded mode, trust the bytes (legacy behavior). The
            // payload is everything after the 32-byte digest + 32-byte tag.
            None => frame.get(32..).map(|b| b.to_vec()),
        }
    }
}

/// Load a component, deserializing a precompiled `.cwasm` (see
/// [`precompile_component_to_file`]) instead of Cranelift-compiling a `.wasm`.
/// A `.cwasm` makes even the first run fast (no compile); it is CPU- and
/// wasmtime-version-specific, and `deserialize` validates that before use.
///
/// A `.cwasm` is HMAC-authenticated (compose-core `CompileCache`) before it is
/// deserialized: a tampered or foreign-engine file is rejected rather than
/// turned into runnable machine code.
fn load_component(engine: &Engine, path: &Path) -> Result<Component> {
    if path.extension().and_then(|s| s.to_str()) == Some("cwasm") {
        let framed = std::fs::read(path)
            .with_context(|| format!("read precompiled {}", path.display()))?;
        let trusted = cwasm_trust::open_cwasm(&framed).ok_or_else(|| {
            anyhow::anyhow!(
                "refusing to load {}: precompiled artifact failed HMAC verification \
                 (tampered or built for a different engine); delete it to recompile",
                path.display()
            )
        })?;
        // SAFETY: the bytes are now authenticated by a per-machine HMAC bound
        // to this engine identity; deserialize additionally checks
        // version/config and does not execute the contents.
        unsafe { Component::deserialize(engine, &trusted) }
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
///
/// The `.cwasm` is written as a self-authenticating frame
/// (`source_wasm_digest || hmac_tag || precompiled`) so [`load_component`] can
/// verify it before deserializing.
pub fn precompile_component_to_file(in_path: &Path, out_path: &Path) -> Result<()> {
    let engine = build_engine()?;
    let bytes =
        std::fs::read(in_path).with_context(|| format!("read {}", in_path.display()))?;
    let precompiled = engine
        .precompile_component(&bytes)
        .map_err(|e| e.context(format!("precompile {}", in_path.display())))?;
    let framed = {
        // Prepend the source-wasm sha256 so the loader can reconstruct the
        // CompileCache key from the file alone.
        let mut out = compose_core::blobs::compute_digest(&bytes);
        out.extend_from_slice(&cwasm_trust::seal_cwasm(&bytes, &precompiled));
        out
    };
    std::fs::write(out_path, &framed)
        .with_context(|| format!("write {}", out_path.display()))?;
    Ok(())
}

/// Public wrapper around [`build_engine`] for out-of-module callers (the
/// driver-tool wasmtime path in `driver_exec.rs`) that want the same
/// component-model + exceptions + compile-cache config the CLI uses.
pub fn build_engine_for_driver() -> Result<Engine> {
    build_engine()
}

/// Per-connection state the `driver_exec::DriverConnection` holds when it
/// runs against a *persistent* wasm core (the follow-up to the MVP that
/// spawned a fresh CLI capture per SQL). Opaque outside this crate: the
/// only useful operations are `driver_core_exec` / `driver_core_query`.
///
/// The invariant: `connection` is a live `duckdb:component/database`
/// connection handle inside `core`'s store, valid for the lifetime of
/// this struct. Dropping this struct drops the connection handle (via
/// wasmtime's resource table) and lets the core store deallocate its
/// side-tables.
pub(crate) struct DriverCoreState {
    core: Arc<Mutex<CoreExecution>>,
    connection: wasmtime::component::ResourceAny,
    // Kept alive so the extension registry stays valid across calls.
    _extension_manager: Arc<Mutex<ExtensionManager>>,
}

/// Bring up a fresh wasm core, open a connection to `db_path`, and load
/// the `cron` + `cron_scheduler` extensions. This replaces the MVP's
/// per-call `run_cli_capture` — one wasm instantiation lasts for the
/// life of the returned `DriverCoreState`, and subsequent SQL runs
/// through the same store (which is what makes registered scalars,
/// prepared statements, and the DB connection itself persist).
///
/// `db_path` semantics match the WIT contract: `None` (or an empty
/// string) opens `:memory:`; otherwise the path is interpreted by the
/// core's WASI ctx against `preopens`.
pub(crate) fn open_driver_core(
    engine: &Engine,
    artifacts: &ComponentArtifacts,
    preopens: &[(&Path, &str)],
    db_path: Option<&str>,
) -> Result<DriverCoreState> {
    let core_wasi = build_wasi_ctx_inherit(&[String::from("duckdb-core")], preopens)?;
    let extension_manager = Arc::new(Mutex::new(ExtensionManager::new(engine.clone())));
    let core_exec = instantiate_core(
        engine,
        &artifacts.core_component,
        core_wasi,
        extension_manager.clone(),
    )?;
    let core = Arc::new(Mutex::new(core_exec));
    {
        let mut mgr = extension_manager
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        mgr.attach_core(core.clone());
    }

    // Open the connection.
    let path_owned: Option<String> = db_path
        .filter(|s| !s.is_empty())
        .map(|s| s.to_string());
    let connection = {
        let mut c = core.lock().unwrap_or_else(|e| e.into_inner());
        c.with_database(|guest, store| guest.call_open(store, path_owned.as_deref()))
            .map_err(|trap| anyhow::anyhow!("driver-core: call_open trapped: {trap}"))?
            .map_err(|e| anyhow::anyhow!("driver-core: open failed: {e}"))?
    };

    // Bootstrap: load the two extensions ONCE. Persisting across calls
    // (that's the whole point of moving to a persistent core) means later
    // `cron_next` / `cron_advance` / `cron_due` references just work.
    {
        let mut c = core.lock().unwrap_or_else(|e| e.into_inner());
        let bootstrap = "LOAD cron; LOAD cron_scheduler;";
        c.with_database(|guest, store| guest.call_execute(store, connection.clone(), bootstrap))
            .map_err(|trap| anyhow::anyhow!("driver-core: bootstrap LOAD trapped: {trap}"))?
            .map_err(|e| {
                anyhow::anyhow!("driver-core: bootstrap LOAD failed: {}", core_duckerror_message(e))
            })?;
    }

    Ok(DriverCoreState {
        core,
        connection,
        _extension_manager: extension_manager,
    })
}

/// Execute `sql` on the persistent connection. Returns rows-affected for
/// DML (from DuckDB's `Count` shape) or 0 for DDL / SELECT. On failure
/// returns the DuckDB error text, matching the WIT contract.
pub(crate) fn driver_core_exec(state: &mut DriverCoreState, sql: &str) -> std::result::Result<u64, String> {
    let mut c = state.core.lock().unwrap_or_else(|e| e.into_inner());
    let result = c
        .with_database(|guest, store| guest.call_execute(store, state.connection.clone(), sql))
        .map_err(|trap| format!("driver-core: trap: {trap}"))?;
    match result {
        Ok(qr) => Ok(extract_rows_affected(&qr).unwrap_or(0)),
        Err(e) => Err(core_duckerror_message(e)),
    }
}

/// Run `sql` and return each cell stringified — matches the shape both
/// the wasm driver-tool and `duckdb:extension/nested-exec` consume.
pub(crate) fn driver_core_query(
    state: &mut DriverCoreState,
    sql: &str,
) -> std::result::Result<Vec<Vec<String>>, String> {
    let mut c = state.core.lock().unwrap_or_else(|e| e.into_inner());
    let result = c
        .with_database(|guest, store| guest.call_execute(store, state.connection.clone(), sql))
        .map_err(|trap| format!("driver-core: trap: {trap}"))?;
    match result {
        Ok(qr) => Ok(qr
            .rows
            .into_iter()
            .map(|row| row.iter().map(spi_value_text).collect())
            .collect()),
        Err(e) => Err(core_duckerror_message(e)),
    }
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

/// The repository root the host was built from (compile-time `CARGO_MANIFEST_DIR`).
/// Used to locate the bundled `registry/index.json` + `artifacts/extensions/`
/// when nothing more specific (cwd / env override) applies.
pub fn workspace_root() -> PathBuf {
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

/// First 12 hex chars of a contract digest (for human-readable resolver logs).
fn short_digest(digest: &str) -> String {
    digest.chars().take(12).collect()
}

/// Belt-and-braces identifier gate for the `ducklink_prefix(alias,
/// namespace)` sentinel handlers. Mirrors ducklink-extension's
/// `catalog::is_safe_identifier`: non-empty and ASCII `[A-Za-z0-9_]+`.
///
/// Both the alias and the namespace get spliced directly into DDL
/// (`CREATE SCHEMA {alias}`, `CREATE OR REPLACE MACRO {alias}.{name}(...)
/// AS {namespace}.{name}(...)`, and the `INSERT OR REPLACE INTO
/// ducklink.prefixes` string), so this check MUST pass before either
/// value reaches the deferred DDL builder.
fn is_safe_prefix_identifier(s: &str) -> bool {
    !s.is_empty() && s.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
}

/// Build the `CREATE OR REPLACE MACRO` DDL that aliases the namespace
/// function `<namespace>.<name>` under `<alias>.<name>`. Mirrors the
/// ducklink-extension's `catalog::build_alias_macro` (the extension's
/// shared alias-DDL builder) with the workspace-relevant subset:
///
/// * `scalar` / `scalar_macro` / `macro` — direct scalar-form macro.
/// * `table` / `table_macro` — table-macro that re-selects from the
///   underlying TF.
/// * `aggregate` (single-arg only) — `list_aggregate(list(x), 'ns.name')`
///   scalar-macro wrap. Multi-arg aggregates are skipped (users can call
///   the namespace-qualified form directly).
///
/// Any other type returns `None` (skipped). Non-identifier parameter
/// names are replaced with positional `_a{i}` so the macro binds even if
/// the underlying function reports weird parameter labels.
///
/// Callers MUST validate `alias`, `name`, and `namespace` through
/// [`is_safe_prefix_identifier`] first — this builder splices them
/// straight into DDL.
fn build_prefix_alias_macro(
    ftype: &str,
    alias: &str,
    name: &str,
    namespace: &str,
    params: &[String],
) -> Option<String> {
    let arg_names: Vec<String> = params
        .iter()
        .enumerate()
        .map(|(i, p)| {
            if is_safe_prefix_identifier(p) {
                p.clone()
            } else {
                format!("_a{i}")
            }
        })
        .collect();
    let arg_list = arg_names.join(", ");
    match ftype {
        "scalar" | "scalar_macro" | "macro" => Some(format!(
            "CREATE OR REPLACE MACRO {alias}.{name}({arg_list}) AS \
             {namespace}.{name}({arg_list})"
        )),
        "table" | "table_macro" => Some(format!(
            "CREATE OR REPLACE MACRO {alias}.{name}({arg_list}) AS TABLE \
             SELECT * FROM {namespace}.{name}({arg_list})"
        )),
        "aggregate" => {
            if arg_names.len() != 1 {
                None
            } else {
                let arg = &arg_names[0];
                Some(format!(
                    "CREATE OR REPLACE MACRO {alias}.{name}({arg}) AS \
                     list_aggregate(list({arg}), '{namespace}.{name}')"
                ))
            }
        }
        _ => None,
    }
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
        // @5.0.0: first-class 128-bit integer arms carry (lower, upper) halves.
        core_types::Duckvalue::Hugeint(h) => extension_types::Duckvalue::Hugeint(
            extension_types::Hugeintvalue { lower: h.lower, upper: h.upper },
        ),
        core_types::Duckvalue::Uhugeint(h) => extension_types::Duckvalue::Uhugeint(
            extension_types::Uhugeintvalue { lower: h.lower, upper: h.upper },
        ),
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
        // T2-1 residual (major-5): HUGEINT / UHUGEINT scalar values gained
        // first-class arms on the extension side; core is @4.0.0 and only has
        // the `complex(complexvalue)` escape hatch. Serialize the 128-bit
        // integer as a base-10 string (the lossless representation DuckDB
        // exchanges HUGEINT literals in) and label it via the type-expr.
        extension_types::Duckvalue::Hugeint(h) => {
            // Reassemble per the WIT comment: (upper as i128) << 64 | lower.
            let v: i128 = ((h.upper as i128) << 64) | (h.lower as i128);
            core_types::Duckvalue::Complex(core_types::Complexvalue {
                type_expr: "HUGEINT".into(),
                json: v.to_string(),
            })
        }
        extension_types::Duckvalue::Uhugeint(h) => {
            let v: u128 = ((h.upper as u128) << 64) | (h.lower as u128);
            core_types::Duckvalue::Complex(core_types::Complexvalue {
                type_expr: "UHUGEINT".into(),
                json: v.to_string(),
            })
        }
    }
}

// ---------------------------------------------------------------------------
// major-4 columnar core<->extension bridge helpers
// ---------------------------------------------------------------------------
// The wasm core calls callback-dispatch columnar (colvecs of CORE bindgen
// types). The host routes to the extension via the manager's row-major dispatch
// (which re-pivots to the extension's colvec ABI), so these helpers pivot CORE
// colvecs <-> CORE rows and convert CORE <-> EXTENSION values.

/// Read row `r` of a CORE colvec as a CORE `Duckvalue` (validity-aware).
fn core_colvec_value_at(c: &core_callback_dispatch::Colvec, r: usize) -> core_types::Duckvalue {
    use core_column_types::Column;
    let valid = c.validity.is_empty()
        || (r >> 3 >= c.validity.len())
        || (c.validity[r >> 3] >> (r & 7)) & 1 != 0;
    if !valid {
        return core_types::Duckvalue::Null;
    }
    match &c.data {
        Column::Boolean(v) => core_types::Duckvalue::Boolean(v[r]),
        Column::Int64(v) => core_types::Duckvalue::Int64(v[r]),
        Column::Uint64(v) => core_types::Duckvalue::Uint64(v[r]),
        Column::Float64(v) => core_types::Duckvalue::Float64(v[r]),
        Column::Int32(v) => core_types::Duckvalue::Int32(v[r]),
        Column::Int16(v) => core_types::Duckvalue::Int16(v[r]),
        Column::Int8(v) => core_types::Duckvalue::Int8(v[r]),
        Column::Uint32(v) => core_types::Duckvalue::Uint32(v[r]),
        Column::Uint16(v) => core_types::Duckvalue::Uint16(v[r]),
        Column::Uint8(v) => core_types::Duckvalue::Uint8(v[r]),
        Column::Float32(v) => core_types::Duckvalue::Float32(v[r]),
        Column::Timestamp(v) => core_types::Duckvalue::Timestamp(v[r]),
        Column::Time(v) => core_types::Duckvalue::Time(v[r]),
        Column::Timestamptz(v) => core_types::Duckvalue::Timestamptz(v[r]),
        Column::Date(v) => core_types::Duckvalue::Date(v[r]),
        Column::Text(v) => core_types::Duckvalue::Text(v[r].clone()),
        Column::Blob(v) => core_types::Duckvalue::Blob(v[r].clone()),
        Column::Decimal(v) => core_types::Duckvalue::Decimal(core_types::Decimalvalue {
            lower: v[r].lower, upper: v[r].upper, width: v[r].width, scale: v[r].scale,
        }),
        Column::Interval(v) => core_types::Duckvalue::Interval(core_types::Intervalvalue {
            months: v[r].months, days: v[r].days, micros: v[r].micros,
        }),
        Column::Uuid(v) => core_types::Duckvalue::Uuid(core_types::Uuidvalue { hi: v[r].hi, lo: v[r].lo }),
        // @5.0.0: first-class hugeint columns + nested logical arms
        // (list/struct/map/array). Nested arms have no first-class Duckvalue
        // representation; escape them through Complex(json).
        Column::Hugeint(v) => core_types::Duckvalue::Hugeint(core_types::Hugeintvalue {
            lower: v[r].lower, upper: v[r].upper,
        }),
        Column::Uhugeint(v) => core_types::Duckvalue::Uhugeint(core_types::Uhugeintvalue {
            lower: v[r].lower, upper: v[r].upper,
        }),
        // @5.0.0 S1 nested arms: payload is an opaque byte buffer
        // (list-col/struct-col wrap `nested-column { encoded }`, map-col wraps
        // `map-column { keys-encoded, vals-encoded }`, array-col wraps
        // `array-column { size, encoded }`). These have no first-class
        // scalar Duckvalue projection at this cross-boundary point --
        // escape through Complex(json) carrying byte lengths.
        Column::ListCol(v) => core_types::Duckvalue::Complex(core_types::Complexvalue {
            type_expr: format!("LIST(row {r})"),
            json: format!("{{\"kind\":\"list\",\"bytes\":{}}}", v.encoded.len()),
        }),
        Column::StructCol(v) => core_types::Duckvalue::Complex(core_types::Complexvalue {
            type_expr: format!("STRUCT(row {r})"),
            json: format!("{{\"kind\":\"struct\",\"bytes\":{}}}", v.encoded.len()),
        }),
        Column::MapCol(v) => core_types::Duckvalue::Complex(core_types::Complexvalue {
            type_expr: format!("MAP(row {r})"),
            json: format!(
                "{{\"kind\":\"map\",\"keys_bytes\":{},\"vals_bytes\":{}}}",
                v.keys_encoded.len(),
                v.vals_encoded.len()
            ),
        }),
        Column::ArrayCol(v) => core_types::Duckvalue::Complex(core_types::Complexvalue {
            type_expr: format!("ARRAY(row {r})"),
            json: format!(
                "{{\"kind\":\"array\",\"size\":{},\"bytes\":{}}}",
                v.size,
                v.encoded.len()
            ),
        }),
        Column::Complex(v) => core_types::Duckvalue::Complex(core_types::Complexvalue {
            type_expr: v[r].type_expr.clone(), json: v[r].json.clone(),
        }),
    }
}

/// Pivot CORE colvecs to EXTENSION row-major batch (for the manager dispatch).
fn core_colvecs_to_ext_rows(
    args: &[core_callback_dispatch::Colvec],
) -> Vec<Vec<extension_types::Duckvalue>> {
    let n = args.first().map(|c| c.rows as usize).unwrap_or(0);
    (0..n)
        .map(|r| {
            args.iter()
                .map(|c| convert_core_duckvalue_to_extension(core_colvec_value_at(c, r)))
                .collect()
        })
        .collect()
}

/// Build a CORE colvec from EXTENSION result values (arm from the first non-null;
/// NULLs cleared in the out-of-band validity bitmap).
fn ext_values_to_core_colvec(vals: Vec<extension_types::Duckvalue>) -> core_callback_dispatch::Colvec {
    use core_column_types::Column;
    use core_types::Duckvalue as D;
    let core_vals: Vec<D> = vals.into_iter().map(convert_extension_duckvalue_to_core).collect();
    let n = core_vals.len();
    let mut validity: Vec<u8> = Vec::new();
    let mut mark_null = |row: usize, validity: &mut Vec<u8>| {
        if validity.is_empty() {
            *validity = vec![0xFFu8; (n + 7) / 8];
        }
        validity[row >> 3] &= !(1u8 << (row & 7));
    };
    let rep = core_vals.iter().find(|v| !matches!(v, D::Null));
    macro_rules! build {
        ($arm:ident, $default:expr, $pat:pat => $extract:expr) => {{
            let mut out = Vec::with_capacity(n);
            for (r, v) in core_vals.iter().enumerate() {
                match v { $pat => out.push($extract), _ => { mark_null(r, &mut validity); out.push($default); } }
            }
            Column::$arm(out)
        }};
    }
    let data = match rep {
        None => { for r in 0..n { mark_null(r, &mut validity); } Column::Int64(vec![0i64; n]) }
        Some(D::Boolean(_)) => build!(Boolean, false, D::Boolean(x) => *x),
        Some(D::Int64(_)) => build!(Int64, 0i64, D::Int64(x) => *x),
        Some(D::Uint64(_)) => build!(Uint64, 0u64, D::Uint64(x) => *x),
        Some(D::Float64(_)) => build!(Float64, 0.0f64, D::Float64(x) => *x),
        Some(D::Int32(_)) => build!(Int32, 0i32, D::Int32(x) => *x),
        Some(D::Int16(_)) => build!(Int16, 0i16, D::Int16(x) => *x),
        Some(D::Int8(_)) => build!(Int8, 0i8, D::Int8(x) => *x),
        Some(D::Uint32(_)) => build!(Uint32, 0u32, D::Uint32(x) => *x),
        Some(D::Uint16(_)) => build!(Uint16, 0u16, D::Uint16(x) => *x),
        Some(D::Uint8(_)) => build!(Uint8, 0u8, D::Uint8(x) => *x),
        Some(D::Float32(_)) => build!(Float32, 0.0f32, D::Float32(x) => *x),
        Some(D::Timestamp(_)) => build!(Timestamp, 0i64, D::Timestamp(x) => *x),
        Some(D::Time(_)) => build!(Time, 0i64, D::Time(x) => *x),
        Some(D::Timestamptz(_)) => build!(Timestamptz, 0i64, D::Timestamptz(x) => *x),
        Some(D::Date(_)) => build!(Date, 0i32, D::Date(x) => *x),
        Some(D::Text(_)) => build!(Text, String::new(), D::Text(x) => x.clone()),
        Some(D::Blob(_)) => build!(Blob, Vec::new(), D::Blob(x) => x.clone()),
        Some(D::Decimal(_)) => build!(Decimal, core_column_types::Decimalvalue { lower: 0, upper: 0, width: 0, scale: 0 }, D::Decimal(d) => core_column_types::Decimalvalue { lower: d.lower, upper: d.upper, width: d.width, scale: d.scale }),
        Some(D::Interval(_)) => build!(Interval, core_column_types::Intervalvalue { months: 0, days: 0, micros: 0 }, D::Interval(d) => core_column_types::Intervalvalue { months: d.months, days: d.days, micros: d.micros }),
        Some(D::Uuid(_)) => build!(Uuid, core_column_types::Uuidvalue { hi: 0, lo: 0 }, D::Uuid(d) => core_column_types::Uuidvalue { hi: d.hi, lo: d.lo }),
        // @5.0.0: first-class 128-bit integer columnar arms.
        Some(D::Hugeint(_)) => build!(Hugeint, core_column_types::DuckInt128 { lower: 0, upper: 0 }, D::Hugeint(h) => core_column_types::DuckInt128 { lower: h.lower, upper: h.upper }),
        Some(D::Uhugeint(_)) => build!(Uhugeint, core_column_types::DuckUint128 { lower: 0, upper: 0 }, D::Uhugeint(h) => core_column_types::DuckUint128 { lower: h.lower, upper: h.upper }),
        Some(D::Complex(_)) => build!(Complex, core_column_types::Complexvalue { type_expr: String::new(), json: "null".into() }, D::Complex(c) => core_column_types::Complexvalue { type_expr: c.type_expr.clone(), json: c.json.clone() }),
        Some(D::Null) => unreachable!(),
    };
    core_callback_dispatch::Colvec { data, validity, rows: n as u32 }
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
        // @5.0.0: DECIMAL carries decimalshape { width, scale } payload on
        // both sides -- pass through structurally.
        extension_types::Logicaltype::Decimal(shape) => {
            core_types::Logicaltype::Decimal(core_types::Decimalshape {
                width: shape.width,
                scale: shape.scale,
            })
        }
        extension_types::Logicaltype::Interval => core_types::Logicaltype::Interval,
        extension_types::Logicaltype::Uuid => core_types::Logicaltype::Uuid,
        // @5.0.0: first-class HUGEINT / UHUGEINT arms on both sides.
        extension_types::Logicaltype::Hugeint => core_types::Logicaltype::Hugeint,
        extension_types::Logicaltype::Uhugeint => core_types::Logicaltype::Uhugeint,
        extension_types::Logicaltype::Complex(expr) => core_types::Logicaltype::Complex(expr),
    }
}

fn convert_extension_columndef_to_core(col: extension_types::Columndef) -> core_types::Columndef {
    core_types::Columndef {
        name: col.name,
        logical: convert_extension_logicaltype_to_core(col.logical),
    }
}

// M2c: inverse mapping used by the write-side storage-host imports (create-table,
// insert-rows, update-rows) to hand the core -> extension trampoline the same
// Logicaltype / Columndef shape ExtensionInstance expects.
fn convert_core_logicaltype_to_extension(
    ty: core_types::Logicaltype,
) -> extension_types::Logicaltype {
    match ty {
        core_types::Logicaltype::Boolean => extension_types::Logicaltype::Boolean,
        core_types::Logicaltype::Int64 => extension_types::Logicaltype::Int64,
        core_types::Logicaltype::Uint64 => extension_types::Logicaltype::Uint64,
        core_types::Logicaltype::Float64 => extension_types::Logicaltype::Float64,
        core_types::Logicaltype::Text => extension_types::Logicaltype::Text,
        core_types::Logicaltype::Blob => extension_types::Logicaltype::Blob,
        core_types::Logicaltype::Int32 => extension_types::Logicaltype::Int32,
        core_types::Logicaltype::Timestamp => extension_types::Logicaltype::Timestamp,
        core_types::Logicaltype::Int8 => extension_types::Logicaltype::Int8,
        core_types::Logicaltype::Int16 => extension_types::Logicaltype::Int16,
        core_types::Logicaltype::Uint8 => extension_types::Logicaltype::Uint8,
        core_types::Logicaltype::Uint16 => extension_types::Logicaltype::Uint16,
        core_types::Logicaltype::Uint32 => extension_types::Logicaltype::Uint32,
        core_types::Logicaltype::Float32 => extension_types::Logicaltype::Float32,
        core_types::Logicaltype::Date => extension_types::Logicaltype::Date,
        core_types::Logicaltype::Time => extension_types::Logicaltype::Time,
        core_types::Logicaltype::Timestamptz => extension_types::Logicaltype::Timestamptz,
        // @5.0.0: DECIMAL carries decimalshape { width, scale } on both sides.
        core_types::Logicaltype::Decimal(shape) => {
            extension_types::Logicaltype::Decimal(extension_types::Decimalshape {
                width: shape.width,
                scale: shape.scale,
            })
        }
        core_types::Logicaltype::Interval => extension_types::Logicaltype::Interval,
        core_types::Logicaltype::Uuid => extension_types::Logicaltype::Uuid,
        // @5.0.0: first-class HUGEINT / UHUGEINT on both sides.
        core_types::Logicaltype::Hugeint => extension_types::Logicaltype::Hugeint,
        core_types::Logicaltype::Uhugeint => extension_types::Logicaltype::Uhugeint,
        core_types::Logicaltype::Complex(expr) => extension_types::Logicaltype::Complex(expr),
    }
}

fn convert_core_columndef_to_extension(col: core_types::Columndef) -> extension_types::Columndef {
    extension_types::Columndef {
        name: col.name,
        logical: convert_core_logicaltype_to_extension(col.logical),
    }
}

// Phase 2 (@5): translators from core-WIT storage-host scan types to the
// dispatch-side storage-interface types are DELETED alongside the storage-host
// import itself. The host now BUILDS scan-requests directly from its ATTACH
// intercept (in HostState::execute) rather than translating what the core
// pushed. See ADR Decision 3 + Amendment A1.

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
        // @5.0.0: first-class 128-bit integer values.
        core_types::Duckvalue::Hugeint(h) => format_hugeint(h.lower, h.upper),
        core_types::Duckvalue::Uhugeint(h) => format_uhugeint(h.lower, h.upper),
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
        // nested-exec Direction-1 §5.(b.1): a shared sibling-core state
        // pinned to the same core component + the SAME preopens the primary
        // received (so the sibling resolves user-facing paths identically).
        // `open` writes the primary's path here; the first `nested_exec`
        // lazy-inits the sibling.
        //
        // Phase 4: pass the primary's ExtensionManager into the sibling so the
        // sibling's `callback-dispatch` host import routes to the primary's
        // loaded extension instances (see `SiblingState` doc + ADR Decision 6).
        let sibling = Arc::new(SiblingState::new(
            engine.clone(),
            artifacts.core_component.clone(),
            owned_preopens.clone(),
            Some(extension_manager.clone()),
        ));
        // Phase 4 follow-up (FU4): give the primary's `CoreStoreState` the
        // shared archive so every post-LOAD drain appends a snapshot. When a
        // sibling later boots, its `get_pending_registrations` reads from this
        // same Arc — the sibling's DuckDB catalog then acquires the same UDFs
        // the primary registered, letting `nested-exec` SQL reach extension
        // scalars/tables/aggregates.
        {
            let mut c = core.lock().unwrap_or_else(|e| e.into_inner());
            c.attach_replay_archive(sibling.replay_archive.clone(), false);
        }
        {
            let mut manager = extension_manager
                .lock()
                .expect("extension manager mutex poisoned");
            manager.attach_core(core.clone());
            manager.attach_sibling_state(sibling.clone());
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
            wasi_http: WasiHttpCtx::new(),
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
            sibling: Some(sibling),
            attached_aliases: HashMap::new(),
        };
        let mut store = Store::new(&engine, host_state);

        let mut linker = Linker::<HostState>::new(&engine);
        p2::add_to_linker_sync(&mut linker)?;
        add_wasi_http_to_linker(&mut linker)?;
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

/// Route A: run the REAL DuckDB shell as a `wasi:cli/run` command component that
/// imports the componentized-extension surface, so `LOAD <name>` inside the
/// shell dispatches shell -> ducklink runtime -> resident extension wasm.
///
/// Unlike `run_cli_with_stdio` (which drives the composed core's `database`
/// interface from a thin CLI front-end), the shell IS the engine: it statically
/// links DuckDB and registers a loaded extension's scalar functions onto its own
/// connection (via the shell-glue install glue + the db-handle bridge). The host
/// still instantiates the composed core, but only to provide `CoreServices`
/// (config/logging/live-query) to the extension loader during `LOAD`.
///
/// The shell command's imports (host-extension-loader, extension-loader-hooks,
/// callback-dispatch@2.0.0) are a subset of the core world, so the linker reuses
/// `CoreStoreState` and the exact same `add_to_linker` wiring `instantiate_core`
/// uses. stdio is inherited (the TTY) instead of driving `database`.
pub fn run_shell_with_stdio(
    shell_component: &Path,
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
    let shell_wasi = build_wasi_ctx_inherit(&args_vec, &preopen_refs)?;
    let core_wasi = build_wasi_ctx_inherit(&[String::from("duckdb-core")], &preopen_refs)?;

    // The composed core is instantiated purely as the CoreServices provider
    // (config/logging/live-query) for extension LOADs; the SHELL runs queries.
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
        manager.attach_current_connection(Arc::new(Mutex::new(None)));
    }

    // CoreStoreState implements the host-extension-loader / extension-loader-hooks
    // / callback-dispatch Host traits via the ExtensionManager -- exactly the
    // imports the shell command declares.
    let mut linker = Linker::<CoreStoreState>::new(&engine);
    p2::add_to_linker_sync(&mut linker)?;
    add_wasi_http_to_linker(&mut linker)?;
    core_host_loader::add_to_linker::<CoreStoreState, CoreStoreState>(&mut linker, |s| s)?;
    core_extension_hooks::add_to_linker::<CoreStoreState, CoreStoreState>(&mut linker, |s| s)?;
    core_callback_dispatch::add_to_linker::<CoreStoreState, CoreStoreState>(&mut linker, |s| s)?;

    let mut store = Store::new(
        &engine,
        CoreStoreState {
            table: ResourceTable::new(),
            wasi: shell_wasi,
            wasi_http: WasiHttpCtx::new(),
            extension_manager: extension_manager.clone(),
            tvm: tvm_core::RegionDirectory::new(),
            tvm_slots: std::collections::HashMap::new(),
            // Phase 4 follow-up (FU4): the standalone-shell driver never
            // constructs a SiblingState, so the archive stays disabled.
            // `is_sibling = false` keeps the shell's core on the historical
            // drain path (identical pre-FU4 behaviour).
            replay_archive: None,
            is_sibling: false,
        },
    );

    let component = load_component(&engine, shell_component).with_context(|| {
        format!(
            "failed to load shell component from {}",
            shell_component.display()
        )
    })?;
    let instance = linker.instantiate(store.as_context_mut(), &component)?;
    let (_, run_iface) = instance
        .get_export(store.as_context_mut(), None, "wasi:cli/run@0.2.0")
        .context("shell component missing wasi:cli/run@0.2.0 export")?;
    let (_, run_idx) = instance
        .get_export(store.as_context_mut(), Some(&run_iface), "run")
        .context("shell component missing run function")?;
    let run = instance
        .get_typed_func::<(), (Result<(), ()>,)>(store.as_context_mut(), run_idx)?;
    let (result,) = run.call(store.as_context_mut(), ())?;
    Ok(result)
}

pub fn run_cli_with_stdio(
    artifacts: &ComponentArtifacts,
    args: &[impl AsRef<str>],
    preopens: &[(&Path, &str)],
) -> Result<Result<(), ()>> {
    let owned_preopens = resolve_preopens_with_default(preopens)?;
    let preopen_refs: Vec<(&Path, &str)> = owned_preopens
        .iter()
        .map(|(host, guest)| (host.as_path(), guest.as_str()))
        .collect();
    let args_vec: Vec<String> = args.iter().map(|s| s.as_ref().to_owned()).collect();
    let cli_wasi = build_wasi_ctx_inherit(&args_vec, &preopen_refs)?;
    run_cli_inner(artifacts, owned_preopens, cli_wasi)
}

/// Like `run_cli_with_stdio`, but drives the CLI with in-memory stdio: `stdin`
/// is fed `stdin_bytes` (e.g. a SQL script) and the CLI's stdout is captured and
/// returned. For in-process query execution (no subprocess). Host-side log lines
/// (extension loading, etc.) still go to the process's real stderr via
/// `eprintln`, not through this captured pipe.
pub fn run_cli_capture(
    artifacts: &ComponentArtifacts,
    args: &[impl AsRef<str>],
    preopens: &[(&Path, &str)],
    stdin_bytes: &[u8],
) -> Result<String> {
    let owned_preopens = resolve_preopens_with_default(preopens)?;
    let preopen_refs: Vec<(&Path, &str)> = owned_preopens
        .iter()
        .map(|(host, guest)| (host.as_path(), guest.as_str()))
        .collect();
    let args_vec: Vec<String> = args.iter().map(|s| s.as_ref().to_owned()).collect();
    let stdin = MemoryInputPipe::new(stdin_bytes.to_vec());
    let stdout = MemoryOutputPipe::new(usize::MAX);
    let stderr = MemoryOutputPipe::new(usize::MAX);
    let cli_wasi =
        build_wasi_ctx_with_pipes(&args_vec, &preopen_refs, stdin, stdout.clone(), stderr)?;
    // The CLI's own exit Result is irrelevant here; we want its captured output.
    let _ = run_cli_inner(artifacts, owned_preopens, cli_wasi)?;
    Ok(String::from_utf8_lossy(&stdout.contents()).into_owned())
}

/// Shared core of the CLI run path: instantiate the composed core + dotcmd
/// registry, wire the linker, and drive the CLI component's `wasi:cli/run`.
/// `cli_wasi` carries the CLI's stdio — inherited (`run_cli_with_stdio`) or
/// in-memory pipes (`run_cli_capture`).
fn run_cli_inner(
    artifacts: &ComponentArtifacts,
    owned_preopens: Vec<(PathBuf, String)>,
    cli_wasi: WasiCtx,
) -> Result<Result<(), ()>> {
    let engine = build_engine()?;
    let preopen_refs: Vec<(&Path, &str)> = owned_preopens
        .iter()
        .map(|(host, guest)| (host.as_path(), guest.as_str()))
        .collect();
    let core_wasi = build_wasi_ctx_inherit(&[String::from("duckdb-core")], &preopen_refs)?;

    let extension_manager = Arc::new(Mutex::new(ExtensionManager::new(engine.clone())));
    let core_exec = instantiate_core(
        &engine,
        &artifacts.core_component,
        core_wasi,
        extension_manager.clone(),
    )?;
    let core = Arc::new(Mutex::new(core_exec));
    // nested-exec Direction-1 §5.(b.1): see the mirror block in
    // `CliHarness::with_artifacts`. Phase 4: pass the primary's
    // ExtensionManager (see the mirror comment there for rationale).
    let sibling = Arc::new(SiblingState::new(
        engine.clone(),
        artifacts.core_component.clone(),
        owned_preopens.clone(),
        Some(extension_manager.clone()),
    ));
    // Phase 4 follow-up (FU4): mirror of the block in `CliHarness::with_artifacts`
    // — wire the primary CoreExecution to the shared replay archive so the
    // sibling can serve extension registrations on its post-LOAD drain.
    {
        let mut c = core.lock().unwrap_or_else(|e| e.into_inner());
        c.attach_replay_archive(sibling.replay_archive.clone(), false);
    }
    {
        let mut manager = extension_manager
            .lock()
            .expect("extension manager mutex poisoned");
        manager.attach_core(core.clone());
        manager.attach_sibling_state(sibling.clone());
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
        wasi_http: WasiHttpCtx::new(),
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
        sibling: Some(sibling),
        attached_aliases: HashMap::new(),
    };
    let mut store = Store::new(&engine, host_state);

    let mut linker = Linker::<HostState>::new(&engine);
    p2::add_to_linker_sync(&mut linker)?;
    add_wasi_http_to_linker(&mut linker)?;
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

    #[test]
    fn parser_rewrite_guard_rejects_loops_and_empty() {
        // Empty / whitespace-only rewrite is rejected.
        assert!(validate_parser_rewrite("ggsql", "VISUALIZE x", "").is_err());
        assert!(validate_parser_rewrite("ggsql", "VISUALIZE x", "   \n\t").is_err());
        // A rewrite identical to the input (modulo surrounding whitespace) is the
        // simplest re-plan loop -> rejected.
        assert!(validate_parser_rewrite("ggsql", "VISUALIZE x", "VISUALIZE x").is_err());
        assert!(validate_parser_rewrite("ggsql", "  VISUALIZE x  ", "VISUALIZE x").is_err());
        // A genuine rewrite to different SQL is accepted (the core then binds it).
        assert!(validate_parser_rewrite("ggsql", "VISUALIZE x", "SELECT 1").is_ok());
    }

    #[test]
    fn network_grant_adapter_preserves_behavior() {
        use datalink_policy::Capability;
        let granted = |g: Option<&str>, ext: &str| {
            network_grant_policy_for(g, ext).is_granted(Capability::Http)
        };
        // unset / none / empty -> deny every extension.
        assert!(!granted(None, "dns"));
        assert!(!granted(Some("none"), "dns"));
        assert!(!granted(Some(""), "dns"));
        // all / * -> grant every extension.
        assert!(granted(Some("all"), "dns"));
        assert!(granted(Some("*"), "anything"));
        // allowlist gates by name (case-insensitive); others denied.
        assert!(granted(Some("dns,http"), "http"));
        assert!(granted(Some("dns, http"), "DNS"));
        assert!(!granted(Some("dns,http"), "azure"));
    }

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
        add_wasi_http_to_linker(&mut linker)?;
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
        wasi_http: WasiHttpCtx,
        next_resource_id: u32,
    }

    impl TestExtensionHost {
        fn new() -> Self {
            let wasi = WasiCtxBuilder::new().inherit_env().inherit_stdio().build();
            Self {
                table: ResourceTable::new(),
                wasi,
                wasi_http: WasiHttpCtx::new(),
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

    impl WasiHttpView for TestExtensionHost {
        fn http(&mut self) -> WasiHttpCtxView<'_> {
            WasiHttpCtxView {
                ctx: &mut self.wasi_http,
                table: &mut self.table,
                hooks: Default::default(),
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
            L::Decimal { width: 18, scale: 3 },
            L::Interval,
            L::Uuid,
            L::Hugeint,
            L::UHugeint,
            L::List(Box::new(L::Int32)),
            L::Struct(vec![("a".into(), L::Int32)]),
            L::Map(Box::new(L::Text), Box::new(L::Int32)),
            L::Array(4, Box::new(L::Int32)),
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

    // Phase 2 (@5): `convert_core_duckvalue_to_storage` was deleted along
    // with the storage-host import; the host builds scan-request values from
    // its ATTACH intercept now. Test removed.

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

    // ------------------------------------------------------------------
    // nested-exec Direction-1 §5.(b.1) sibling-core tests.
    //
    // These drive `CoreServices::nested_exec` (the ExtensionServices sink the
    // component-side `duckdb:extension/nested-exec` import routes to) against
    // a real sibling core opened over a temp file-backed DuckDB. No
    // extensions are loaded on the sibling — that's the whole limitation the
    // heuristic in `is_extension_related_error` gates the Direction-2
    // redirect against.
    // ------------------------------------------------------------------

    /// Build a `CoreServices` wired to a fresh sibling that preopens
    /// `preopen_dir` at guest path `.` and records the guest-visible
    /// `db_guest_path` (e.g. `"./mydb.duckdb"`) as the primary's DB path.
    /// The `primary` [`CoreExecution`] is a throwaway (`nested_exec` never
    /// touches it) — just enough to satisfy the struct's `core` field.
    fn build_direction1_services(
        artifacts: &ComponentArtifacts,
        preopen_dir: &Path,
        db_guest_path: &str,
    ) -> Result<(Arc<Mutex<CoreExecution>>, Arc<SiblingState>, CoreServices)> {
        let engine = build_engine()?;
        // Throwaway primary — nested_exec never reads it, but the struct
        // holds an Arc<Mutex<CoreExecution>> so we build a fresh one.
        let primary_wasi =
            build_wasi_ctx_inherit(&[String::from("duckdb-core-primary-throwaway")], &[])?;
        let primary_manager = Arc::new(Mutex::new(ExtensionManager::new(engine.clone())));
        let primary_core = Arc::new(Mutex::new(instantiate_core(
            &engine,
            &artifacts.core_component,
            primary_wasi,
            primary_manager,
        )?));

        // Sibling gets a single preopen so it can reach the file the test
        // wrote at `preopen_dir/db_guest_path`. `None` extension_manager
        // preserves the historical fresh-sibling behaviour these tests were
        // written against (Phase 4's shared-manager path is exercised by
        // `tests/test_phase4_sibling_ext_nested_exec.rs`).
        let preopens = vec![(preopen_dir.to_path_buf(), ".".to_string())];
        let sibling = Arc::new(SiblingState::new(
            engine.clone(),
            artifacts.core_component.clone(),
            preopens,
            None,
        ));
        // The test skips going through HostState::open, so record the DB path
        // directly (exactly what `open`/`open_with_config` do in the CLI path).
        sibling.record_primary_open(Some(db_guest_path.to_string()));

        let services = CoreServices {
            core: primary_core.clone(),
            current_connection: Arc::new(Mutex::new(None)),
            catalog_snapshot: Arc::new(Mutex::new(CatalogSnapshot::default())),
            sibling: Some(sibling.clone()),
        };
        Ok((primary_core, sibling, services))
    }

    #[test]
    fn nested_exec_direction1_select_returns_rows() -> Result<()> {
        let artifacts = ComponentArtifacts::resolve_default()?;
        let tmp = tempdir()?;
        let (_primary, _sibling, mut services) =
            build_direction1_services(&artifacts, tmp.path(), "./d1-select.duckdb")?;

        let r = services
            .nested_exec("SELECT 42 AS x")
            .expect("SELECT nested_exec ok");
        let rows = r.rows.expect("SELECT populates rows");
        assert_eq!(rows.len(), 1, "one row expected, got {rows:?}");
        assert_eq!(rows[0].len(), 1, "one cell expected, got {:?}", rows[0]);
        assert_eq!(rows[0][0], "42");
        assert!(r.rows_affected.is_none(), "SELECT should not report rows_affected");
        Ok(())
    }

    #[test]
    fn nested_exec_direction1_ddl_and_dml() -> Result<()> {
        let artifacts = ComponentArtifacts::resolve_default()?;
        let tmp = tempdir()?;
        let (_primary, _sibling, mut services) =
            build_direction1_services(&artifacts, tmp.path(), "./d1-ddl-dml.duckdb")?;

        // CREATE TABLE (DDL). No rows, no rows_affected expected.
        let create = services
            .nested_exec("CREATE TABLE t (x INT)")
            .expect("CREATE TABLE nested_exec ok");
        assert!(
            create.rows.as_ref().map(|r| r.is_empty()).unwrap_or(true),
            "CREATE TABLE should not produce user rows, got {create:?}"
        );

        // INSERT (DML). DuckDB emits a single `Count`-column row for pure DML;
        // we lift it into rows_affected.
        let insert = services
            .nested_exec("INSERT INTO t VALUES (1),(2)")
            .expect("INSERT nested_exec ok");
        assert_eq!(
            insert.rows_affected,
            Some(2),
            "INSERT should report 2 rows_affected, got {insert:?}"
        );

        // SELECT count(*). Row-producing; check the cell.
        let sel = services
            .nested_exec("SELECT count(*) FROM t")
            .expect("SELECT count(*) nested_exec ok");
        let rows = sel.rows.expect("SELECT populates rows");
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0][0], "2");
        Ok(())
    }

    #[test]
    fn nested_exec_direction1_missing_extension_function() -> Result<()> {
        let artifacts = ComponentArtifacts::resolve_default()?;
        let tmp = tempdir()?;
        let (_primary, _sibling, mut services) =
            build_direction1_services(&artifacts, tmp.path(), "./d1-ext.duckdb")?;

        // Reference an obviously-missing function. The primary might have
        // extension-provided scalars via DUCKLINK_AUTOLOAD, but the SIBLING
        // core loads none — so any function not in the DuckDB built-ins
        // triggers the extension-related-error redirect.
        let err = services
            .nested_exec("SELECT some_missing_scalar_function('x')")
            .expect_err("missing function should fail");
        assert!(
            err.starts_with("nested-exec (Direction 1):"),
            "expected Direction-1 redirect prefix, got: {err}"
        );
        assert!(
            err.contains("native ducklink DuckDB extension (Direction 2)"),
            "expected Direction-2 pointer in error, got: {err}"
        );
        assert!(
            err.contains("Underlying error:"),
            "expected the underlying error to be preserved, got: {err}"
        );
        Ok(())
    }

    #[test]
    fn nested_exec_direction1_syntax_error_passes_through_unchanged() -> Result<()> {
        // Negative case for `is_extension_related_error`: a plain syntax error
        // must NOT be wrapped with the Direction-2 redirect (that would be
        // misleading — it isn't an extension issue).
        let artifacts = ComponentArtifacts::resolve_default()?;
        let tmp = tempdir()?;
        let (_primary, _sibling, mut services) =
            build_direction1_services(&artifacts, tmp.path(), "./d1-syntax.duckdb")?;

        let err = services
            .nested_exec("SELEKT 1")
            .expect_err("syntax error should fail");
        assert!(
            !err.starts_with("nested-exec (Direction 1):"),
            "syntax error must not be tagged Direction-2, got: {err}"
        );
        Ok(())
    }

    #[test]
    fn nested_exec_direction1_in_memory_primary_errors_clearly() -> Result<()> {
        let artifacts = ComponentArtifacts::resolve_default()?;
        let tmp = tempdir()?;
        let (_primary, sibling, mut services) =
            build_direction1_services(&artifacts, tmp.path(), "./d1-mem.duckdb")?; // path unused

        // Override the record: pretend the primary opened `:memory:`.
        sibling.record_primary_open(None);

        let err = services
            .nested_exec("SELECT 1")
            .expect_err(":memory: primary should fail nested_exec");
        assert!(
            err.contains("in-memory"),
            "expected in-memory explanation, got: {err}"
        );
        Ok(())
    }

    #[test]
    fn nested_exec_direction1_depth_cap() -> Result<()> {
        // Drive the WIT wrapper (`extension_nested_exec::Host::nested_exec`)
        // through a services sink whose own body re-invokes the same wrapper
        // via a raw-pointer stashed thread-local. Each entry bumps the
        // per-thread depth guard; the (NESTED_EXEC_MAX_DEPTH+1)th invocation
        // errors with the depth-cap message BEFORE reaching the sink again.
        //
        // Runtime-crate-side tests cover the guard in isolation
        // (`nested_exec_depth_cap_returns_error_at_level_max_plus_one` in
        // `crates/ducklink-runtime/src/extension.rs`); this test complements
        // it by proving the guard fires when driven end-to-end THROUGH our
        // sink.
        use ducklink_runtime::duckdb_extension_bindings::duckdb::extension::nested_exec as ext_nested_exec;
        use ducklink_runtime::{
            CallbackRegistry, ExtensionServices, ExtensionStoreState, LogField, LogLevel,
            NestedExecResult, NESTED_EXEC_MAX_DEPTH,
        };
        use std::cell::Cell;
        use std::sync::RwLock;

        thread_local! {
            /// Non-null while a depth-cap test is running; the sink reads it
            /// to know which state to recurse into. Set/cleared by the test.
            static RECURSE_STATE: Cell<*mut ExtensionStoreState> = const { Cell::new(std::ptr::null_mut()) };
            /// Last error the sink observed from its own recursive
            /// Host::nested_exec call — the depth-cap message when the guard
            /// fires.
            static RECURSE_LAST_ERR: std::cell::RefCell<Option<String>> = const { std::cell::RefCell::new(None) };
        }

        struct RecursingSink;
        impl ExtensionServices for RecursingSink {
            fn provider_version(&mut self) -> Result<String, ducklink_runtime::ConfigError> {
                Ok("test".to_string())
            }
            fn list_keys(&mut self, _: Option<&str>) -> Result<Vec<String>, ducklink_runtime::ConfigError> {
                Ok(Vec::new())
            }
            fn get_string(&mut self, _: &str) -> Result<Option<String>, ducklink_runtime::ConfigError> {
                Ok(None)
            }
            fn get_bool(&mut self, _: &str) -> Result<Option<bool>, ducklink_runtime::ConfigError> {
                Ok(None)
            }
            fn get_i64(&mut self, _: &str) -> Result<Option<i64>, ducklink_runtime::ConfigError> {
                Ok(None)
            }
            fn get_u64(&mut self, _: &str) -> Result<Option<u64>, ducklink_runtime::ConfigError> {
                Ok(None)
            }
            fn get_f64(&mut self, _: &str) -> Result<Option<f64>, ducklink_runtime::ConfigError> {
                Ok(None)
            }
            fn get_bytes(&mut self, _: &str) -> Result<Option<Vec<u8>>, ducklink_runtime::ConfigError> {
                Ok(None)
            }
            fn get_string_list(&mut self, _: &str) -> Result<Option<Vec<String>>, ducklink_runtime::ConfigError> {
                Ok(None)
            }
            fn log(&mut self, _: LogLevel, _: &str, _: Option<&str>) {}
            fn log_fields(&mut self, _: LogLevel, _: &str, _: &[LogField]) {}
            fn nested_exec(&mut self, _sql: &str) -> Result<NestedExecResult, String> {
                // Re-enter the WIT wrapper. Its `NestedExecDepthGuard::enter()`
                // bumps the thread-local counter; the guard drops when this
                // frame returns.
                let state_ptr = RECURSE_STATE.with(|c| c.get());
                if state_ptr.is_null() {
                    return Ok(NestedExecResult {
                        rows: Some(Vec::new()),
                        rows_affected: None,
                    });
                }
                // SAFETY: single-threaded test scope; `state` outlives this
                // recursive chain (owned by the outer stack frame in
                // `nested_exec_direction1_depth_cap`), the pointer is set
                // before the outermost `Host::nested_exec` call and cleared
                // after — no concurrent access, no lifetime overlap.
                let state: &mut ExtensionStoreState = unsafe { &mut *state_ptr };
                match ext_nested_exec::Host::nested_exec(state, "recurse".to_string()) {
                    Ok(_) => Ok(NestedExecResult {
                        rows: Some(Vec::new()),
                        rows_affected: None,
                    }),
                    Err(e) => {
                        RECURSE_LAST_ERR.with(|c| *c.borrow_mut() = Some(e.clone()));
                        // Bubble the depth-cap error up so the outermost
                        // `Host::nested_exec` returns Err as well.
                        Err(e)
                    }
                }
            }
        }

        let wasi = wasmtime_wasi::WasiCtxBuilder::new().build();
        let mut state = ExtensionStoreState::new(
            wasi,
            Box::new(RecursingSink),
            Arc::new(RwLock::new(CallbackRegistry::default())),
            "depth-cap-test".to_string(),
        );
        RECURSE_STATE.with(|c| c.set(&mut state as *mut _));
        RECURSE_LAST_ERR.with(|c| *c.borrow_mut() = None);

        // Outermost Host::nested_exec: depth 0 -> 1 (enters guard). Recurses
        // NESTED_EXEC_MAX_DEPTH more times; the (max+1)th enter() rejects.
        let err = ext_nested_exec::Host::nested_exec(&mut state, "kick".to_string())
            .expect_err("depth cap must fire at max+1");
        assert!(
            err.contains("max nesting depth"),
            "outer error should carry the depth-cap message; got: {err}"
        );
        // Every recursed frame surfaces the same message.
        let inner = RECURSE_LAST_ERR
            .with(|c| c.borrow().clone())
            .expect("inner recursion should have observed at least one error");
        assert!(
            inner.contains("max nesting depth"),
            "inner error should be the depth-cap message; got: {inner}"
        );
        // The depth ceiling is exposed as a pub const; sanity-check it hasn't
        // shifted so a future NESTED_EXEC_MAX_DEPTH change surfaces here.
        assert!(
            NESTED_EXEC_MAX_DEPTH >= 1,
            "depth cap must be positive"
        );

        RECURSE_STATE.with(|c| c.set(std::ptr::null_mut()));
        Ok(())
    }

    // ------------------------------------------------------------------
    // nested-exec Direction-1 §7.8 option (a): primary-core re-entry tests.
    // ------------------------------------------------------------------
    //
    // These verify the new `PRIMARY_STORE_REENTRY` TLS + `primary_nested_exec`
    // path routes writes through the PRIMARY store (option (a)) instead of
    // the (b.1) sibling — the bug fix for the fieldbook two-catalog problem
    // documented in `docs/nested-exec-direction-1-plan.md` §7.4.

    /// With `PrimaryReentryGuard` installed, `CoreServices::nested_exec`
    /// dispatches to the PRIMARY store + primary connection. A subsequent
    /// direct `call_execute` on the same primary connection sees the write
    /// (the guarantee (b.1) sibling failed to provide).
    #[test]
    fn nested_exec_direction1_reentry_writes_land_on_primary_catalog() -> Result<()> {
        let artifacts = ComponentArtifacts::resolve_default()?;
        let tmp = tempdir()?;
        let engine = build_engine()?;

        // The primary preopens the temp dir at `.` so the guest can `open
        // "./opt-a.duckdb"`. Same shape the CLI uses (see build_wasi_ctx_*).
        let preopens: Vec<(&Path, &str)> = vec![(tmp.path(), ".")];
        let wasi = build_wasi_ctx_inherit(&[String::from("duckdb-core-optA")], &preopens)?;
        let extension_manager = Arc::new(Mutex::new(ExtensionManager::new(engine.clone())));
        let primary_core_exec =
            instantiate_core(&engine, &artifacts.core_component, wasi, extension_manager)?;
        let primary_core = Arc::new(Mutex::new(primary_core_exec));

        // Open a file-backed connection on the PRIMARY. The nested_exec write
        // has to land in this same DB for the test to be meaningful — file
        // sharing across two Databases has WAL semantics that would obscure
        // the catalog-visibility check.
        let db_path = "./opt-a.duckdb";
        let primary_conn = {
            let mut c = primary_core.lock().unwrap();
            c.with_database(|g, s| g.call_open(s, Some(db_path)))?
                .map_err(|e| anyhow::anyhow!("primary open failed: {e}"))?
        };

        // Build a `CoreServices` on the primary. Sibling wired as a safety
        // net (test asserts nested_exec DID NOT go through it by checking
        // the primary catalog). Fresh-manager (`None`) preserves this
        // safety-net semantic (Phase 4's shared-manager path is out of scope
        // for this reentry test).
        let sibling = Arc::new(SiblingState::new(
            engine.clone(),
            artifacts.core_component.clone(),
            vec![(tmp.path().to_path_buf(), ".".to_string())],
            None,
        ));
        sibling.record_primary_open(Some(db_path.to_string()));
        let mut services = CoreServices {
            core: primary_core.clone(),
            current_connection: Arc::new(Mutex::new(Some(primary_conn))),
            catalog_snapshot: Arc::new(Mutex::new(CatalogSnapshot::default())),
            sibling: Some(sibling),
        };

        // Simulate what `HostState::execute` does around the outer
        // `call_execute`: snapshot raw store + bindings pointers and install
        // `PrimaryReentryGuard`. We do NOT drive an outer `call_execute`
        // here — wasmtime tolerates the "nested" call as a plain first call
        // when no outer call is in flight, so this test isolates the
        // dispatch mechanism (guard set -> primary path) from the wasmtime
        // reentrancy question (already answered by `reentrancy_poc.rs`).
        let (store_ptr, bindings_ptr) = {
            let mut c = primary_core.lock().unwrap();
            let store_ptr: *mut Store<CoreStoreState> = &mut c.store;
            let bindings_ptr: *const duckdb_core_bindings::Libduckdb = &c.bindings;
            (store_ptr, bindings_ptr)
        };

        // CREATE TABLE on the primary via nested_exec.
        {
            let _guard = PrimaryReentryGuard::set(PrimaryReentry {
                store: store_ptr,
                bindings: bindings_ptr,
                connection: primary_conn,
            });
            services
                .nested_exec("CREATE TABLE opta_t (x INT)")
                .expect("CREATE TABLE via primary re-entry");
            services
                .nested_exec("INSERT INTO opta_t VALUES (10),(20),(30)")
                .expect("INSERT via primary re-entry");
        }

        // Read back via a direct `call_execute` on the PRIMARY connection.
        // If the write went to the sibling (regression to (b.1)), this
        // catalog wouldn't have the table yet in-process, and the SELECT
        // would fail with `Table with name opta_t does not exist`.
        let sel = {
            let mut c = primary_core.lock().unwrap();
            c.with_database(|g, s| g.call_execute(s, primary_conn, "SELECT count(*) FROM opta_t"))?
                .map_err(|e| anyhow::anyhow!("primary SELECT failed: {e:?}"))?
        };
        assert_eq!(sel.rows.len(), 1, "one count row expected, got {:?}", sel.rows);
        assert_eq!(sel.rows[0].len(), 1);
        let count_cell = spi_value_text(&sel.rows[0][0]);
        assert_eq!(
            count_cell, "3",
            "expected 3 rows on primary catalog (proves the nested_exec write went to \
             the primary, not the sibling); got: {count_cell}"
        );
        Ok(())
    }

    // ------------------------------------------------------------------
    // Phase 4 (@5): sibling core wired to the primary's ExtensionManager.
    //
    // The historical §5.(b.1) sibling built a FRESH `ExtensionManager` for the
    // sibling core, so `nested-exec` SQL that referenced an extension-provided
    // function on the sibling always failed with the
    // `NESTED_EXEC_DIRECTION2_REDIRECT` prefix. Phase 4 threads the primary's
    // `Arc<Mutex<ExtensionManager>>` onto `SiblingState`; when present, the
    // sibling core's `CoreStoreState.extension_manager` binds to the SAME
    // manager as the primary's, so its `callback-dispatch` host imports route
    // to the primary's loaded [`ExtensionInstance`] map + shared
    // [`CallbackRegistry`].
    //
    // These unit tests prove the wiring at the Arc-identity level and check
    // that a plain-SQL sibling call still works when the shared path is taken
    // (regression: no crash on the code path that skips `attach_core` for the
    // sibling). End-to-end extension-scalar dispatch on the sibling depends
    // on additional catalog-registration replay that is out of scope for this
    // Phase; see the `Phase 4 follow-ups` note below.
    // ------------------------------------------------------------------

    /// Build the Phase 4 shared-manager variant of the sibling harness: the
    /// primary's `Arc<Mutex<ExtensionManager>>` is passed onto `SiblingState`,
    /// mirroring what the CLI/harness paths do. Returns the primary core, the
    /// shared manager, the sibling state, and a `CoreServices` sink wired
    /// against them so an outer test can drive `nested_exec` through the
    /// shared path.
    fn build_direction1_services_shared(
        artifacts: &ComponentArtifacts,
        preopen_dir: &Path,
        db_guest_path: &str,
    ) -> Result<(
        Arc<Mutex<CoreExecution>>,
        Arc<Mutex<ExtensionManager>>,
        Arc<SiblingState>,
        CoreServices,
    )> {
        let engine = build_engine()?;
        // Primary is a throwaway (`nested_exec` never touches it — the sibling
        // slot runs the SQL), but its `ExtensionManager` IS shared onto the
        // sibling below, so it's constructed here as the source of truth.
        let primary_wasi = build_wasi_ctx_inherit(
            &[String::from("duckdb-core-primary-shared")],
            &[],
        )?;
        let primary_manager = Arc::new(Mutex::new(ExtensionManager::new(engine.clone())));
        let primary_core = Arc::new(Mutex::new(instantiate_core(
            &engine,
            &artifacts.core_component,
            primary_wasi,
            primary_manager.clone(),
        )?));

        let preopens = vec![(preopen_dir.to_path_buf(), ".".to_string())];
        let sibling = Arc::new(SiblingState::new(
            engine.clone(),
            artifacts.core_component.clone(),
            preopens,
            Some(primary_manager.clone()),
        ));
        sibling.record_primary_open(Some(db_guest_path.to_string()));

        let services = CoreServices {
            core: primary_core.clone(),
            current_connection: Arc::new(Mutex::new(None)),
            catalog_snapshot: Arc::new(Mutex::new(CatalogSnapshot::default())),
            sibling: Some(sibling.clone()),
        };
        Ok((primary_core, primary_manager, sibling, services))
    }

    /// Phase 4 wiring test. Building the sibling harness with a shared
    /// `ExtensionManager` and driving one `nested_exec` (which lazily
    /// materializes the sibling core) must yield a sibling `CoreStoreState`
    /// whose `extension_manager` is Arc-identical to the primary's manager.
    /// If this fails, the sibling would still be routing to a fresh manager
    /// — the whole Phase 4 unblock.
    #[test]
    fn phase4_sibling_binds_primary_extension_manager() -> Result<()> {
        let artifacts = ComponentArtifacts::resolve_default()?;
        let tmp = tempdir()?;
        let (_primary, primary_mgr, sibling, mut services) =
            build_direction1_services_shared(&artifacts, tmp.path(), "./p4-wiring.duckdb")?;

        // Drive the sibling to materialize its slot. Plain SELECT — no
        // callbacks needed, so this exercises the shared path without
        // reaching the primary manager mutex.
        let r = services
            .nested_exec("SELECT 1 AS n")
            .expect("plain SELECT via shared-manager sibling ok");
        let rows = r.rows.expect("SELECT populates rows");
        assert_eq!(rows[0][0], "1", "SELECT 1 must return 1");

        // Inspect the lazily-materialized sibling slot: its `CoreStoreState`
        // must have the PRIMARY's `Arc<Mutex<ExtensionManager>>` as its
        // `extension_manager` field — not a fresh one.
        let slot_guard = sibling.slot.lock().unwrap_or_else(|e| e.into_inner());
        let slot = slot_guard.as_ref().expect("sibling slot populated after nested_exec");
        let sibling_core_guard = slot.core.lock().unwrap_or_else(|e| e.into_inner());
        let sibling_mgr = &sibling_core_guard.store.data().extension_manager;
        assert!(
            Arc::ptr_eq(sibling_mgr, &primary_mgr),
            "sibling CoreStoreState.extension_manager must be Arc-identical to primary's \
             (Phase 4 shared-manager wiring); found different Arcs"
        );
        Ok(())
    }

    /// Phase 4 regression: with shared manager wired, the sibling must NOT
    /// tag catalog-miss errors with the `NESTED_EXEC_DIRECTION2_REDIRECT`
    /// prefix that would only fire when the sibling has no way to reach the
    /// primary's extensions. (Since this test doesn't actually LOAD an
    /// extension, calling a truly-missing scalar still errors — but we check
    /// the shape hasn't regressed: the sibling can drive plain SQL and the
    /// well-formed SELECT still returns a value cleanly.)
    #[test]
    fn phase4_sibling_shared_plain_sql_no_regression() -> Result<()> {
        let artifacts = ComponentArtifacts::resolve_default()?;
        let tmp = tempdir()?;
        let (_primary, _primary_mgr, _sibling, mut services) =
            build_direction1_services_shared(&artifacts, tmp.path(), "./p4-regress.duckdb")?;

        // Plain DDL/DML: no extension callbacks, so the sibling's SQL runs
        // without ever locking the shared manager mutex. This is the
        // fieldbook/mosaic bootstrap-SQL use case Phase 4 targets.
        services
            .nested_exec("CREATE TABLE t (x INT)")
            .expect("CREATE TABLE via shared-manager sibling ok");
        let insert = services
            .nested_exec("INSERT INTO t VALUES (1),(2),(3)")
            .expect("INSERT via shared-manager sibling ok");
        assert_eq!(
            insert.rows_affected,
            Some(3),
            "INSERT should report 3 rows_affected, got {insert:?}"
        );
        let sel = services
            .nested_exec("SELECT count(*) FROM t")
            .expect("SELECT count(*) via shared-manager sibling ok");
        let rows = sel.rows.expect("SELECT populates rows");
        assert_eq!(rows[0][0], "3", "expected 3 rows in the sibling-created table");
        Ok(())
    }

    /// Phase 4 follow-up (FU4) lock-in: prove the sibling's replay archive is
    /// Arc-identical to the primary's write target, that the primary's drain
    /// path appends into it, and that a sibling `CoreStoreState`
    /// materialized by `sibling_ensure_slot` reads back exactly what was
    /// deposited — WITHOUT going through the shared manager mutex.
    ///
    /// This isolates FU4's plumbing from any wasm-shim behaviour: no LOAD is
    /// driven, no real extension is present, we just push a synthetic
    /// `PendingRegistrationsData` into the archive and confirm the sibling
    /// `impl core_extension_hooks::Host` serves it verbatim on
    /// `get_pending_registrations`.
    #[test]
    fn phase4_fu4_sibling_replay_archive_populated_from_primary_drain() -> Result<()> {
        use ducklink_runtime::reg;
        let artifacts = ComponentArtifacts::resolve_default()?;
        let tmp = tempdir()?;
        let (primary_core, primary_mgr, sibling, mut services) =
            build_direction1_services_shared(&artifacts, tmp.path(), "./p4-fu4-archive.duckdb")?;

        // Wire the primary CoreExecution's `replay_archive` field to the
        // SAME Arc the sibling holds — this mirrors the production
        // `run_cli_inner` / `CliHarness::with_artifacts` wiring the FU4
        // patch installed. Without this, the primary would silently drop
        // its drained batch on the floor (archive stays empty).
        {
            let mut c = primary_core.lock().unwrap();
            c.attach_replay_archive(sibling.replay_archive.clone(), false);
        }

        // Seed the manager's `deferred_registrations` with a synthetic
        // scalar + table + aggregate so the next drain has something to
        // return. `deferred_registrations` is prepended verbatim by
        // `drain_pending_registrations` (see the field doc), so this
        // completely sidesteps needing a real ExtensionInstance.
        //
        // Handles are within the extension-callback range (< the top-of-u32
        // sentinels) so no synthetic-sentinel branch fires.
        {
            let mut mgr = primary_mgr.lock().unwrap();
            mgr.deferred_registrations.scalars.push(reg::ScalarReg {
                extension: "fu4_stub".to_string(),
                name: "fu4_stub_scalar".to_string(),
                arguments: vec![reg::FuncArg {
                    name: Some("x".to_string()),
                    logical: reg::LogicalType::Int64,
                }],
                returns: reg::LogicalType::Int64,
                callback_handle: 0x0000_1001,
                options: None,
            });
            mgr.deferred_registrations.tables.push(reg::TableReg {
                extension: "fu4_stub".to_string(),
                name: "fu4_stub_table".to_string(),
                arguments: vec![reg::FuncArg {
                    name: Some("n".to_string()),
                    logical: reg::LogicalType::Int64,
                }],
                columns: vec![reg::ColumnDef {
                    name: "value".to_string(),
                    logical: reg::LogicalType::Text,
                }],
                callback_handle: 0x0000_1002,
                options: None,
            });
            mgr.deferred_registrations
                .aggregates
                .push(reg::AggregateReg {
                    extension: "fu4_stub".to_string(),
                    name: "fu4_stub_agg".to_string(),
                    arguments: vec![reg::FuncArg {
                        name: Some("y".to_string()),
                        logical: reg::LogicalType::Int64,
                    }],
                    returns: reg::LogicalType::Int64,
                    callback_handle: 0x0000_1003,
                    options: None,
                });
            // Record a stub name on the sibling — `sibling_ensure_slot` uses
            // this to pick which `LOAD <name>` to issue. In this test we
            // don't drive that path (no real extension binary), but a
            // non-empty list matches the shape of what
            // `ExtensionManager::ensure_extension_loaded` would deposit.
            sibling.record_extension_loaded("fu4_stub");
        }

        // Trigger the PRIMARY-side drain via its own
        // `core_extension_hooks::Host::get_pending_registrations` impl. This
        // is exactly what the wasm shim would call inside a real LOAD; here
        // we drive it synchronously by borrowing the primary store's data.
        {
            let mut c = primary_core.lock().unwrap();
            let data: &mut CoreStoreState = c.store.data_mut();
            assert!(
                data.replay_archive.is_some(),
                "primary CoreStoreState must have `replay_archive = Some(...)` \
                 after `attach_replay_archive` — FU4 wiring"
            );
            let _returned =
                <CoreStoreState as core_extension_hooks::Host>::get_pending_registrations(data);
        }

        // Inspect the shared archive directly: the synthetic scalar / table
        // / aggregate must be present after the primary drain lands.
        {
            let archive = sibling
                .replay_archive
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            let scalar_names: Vec<&str> =
                archive.scalars.iter().map(|s| s.name.as_str()).collect();
            let table_names: Vec<&str> =
                archive.tables.iter().map(|t| t.name.as_str()).collect();
            let agg_names: Vec<&str> = archive
                .aggregates
                .iter()
                .map(|a| a.name.as_str())
                .collect();
            assert!(
                scalar_names.contains(&"fu4_stub_scalar"),
                "primary drain must have appended scalar into sibling archive; got {scalar_names:?}"
            );
            assert!(
                table_names.contains(&"fu4_stub_table"),
                "primary drain must have appended table into sibling archive; got {table_names:?}"
            );
            assert!(
                agg_names.contains(&"fu4_stub_agg"),
                "primary drain must have appended aggregate into sibling archive; got {agg_names:?}"
            );
        }

        // Materialize the sibling by driving one plain-SQL nested_exec. This
        // takes the shared-manager path (unchanged) and lazily builds the
        // sibling CoreExecution. Post-materialisation, its `CoreStoreState`
        // must carry `is_sibling = true` + the shared `replay_archive`.
        let r = services
            .nested_exec("SELECT 1 AS n")
            .expect("plain SELECT via shared-manager sibling ok");
        let rows = r.rows.expect("SELECT populates rows");
        assert_eq!(rows[0][0], "1");

        // Reach into the sibling slot: `is_sibling` flag set + `replay_archive`
        // Arc-identical to `SiblingState::replay_archive`.
        {
            let slot_guard = sibling.slot.lock().unwrap_or_else(|e| e.into_inner());
            let slot = slot_guard
                .as_ref()
                .expect("sibling slot populated after nested_exec");
            let sibling_core_guard = slot.core.lock().unwrap_or_else(|e| e.into_inner());
            let data = sibling_core_guard.store.data();
            assert!(
                data.is_sibling,
                "sibling CoreStoreState must have `is_sibling = true` \
                 after `attach_replay_archive(_, true)` — FU4 wiring"
            );
            let archive = data
                .replay_archive
                .as_ref()
                .expect("sibling CoreStoreState must have `replay_archive = Some(...)`");
            assert!(
                Arc::ptr_eq(archive, &sibling.replay_archive),
                "sibling's replay_archive must be Arc-identical to \
                 `SiblingState::replay_archive` — FU4 wiring"
            );
        }

        // Now drive the sibling's `get_pending_registrations` directly and
        // confirm it serves the archive (not an empty drain). We call the
        // trait method on the sibling `CoreStoreState` — mirroring what the
        // wasm shim would invoke inside a `LOAD <name>` on the sibling.
        {
            let slot_guard = sibling.slot.lock().unwrap_or_else(|e| e.into_inner());
            let slot = slot_guard.as_ref().unwrap();
            let mut sibling_core = slot.core.lock().unwrap_or_else(|e| e.into_inner());
            let data: &mut CoreStoreState = sibling_core.store.data_mut();
            let served =
                <CoreStoreState as core_extension_hooks::Host>::get_pending_registrations(data);
            let scalar_names: Vec<String> =
                served.scalars.iter().map(|s| s.name.to_string()).collect();
            let table_names: Vec<String> =
                served.tables.iter().map(|t| t.name.to_string()).collect();
            let agg_names: Vec<String> = served
                .aggregates
                .iter()
                .map(|a| a.name.to_string())
                .collect();
            assert!(
                scalar_names.iter().any(|n| n == "fu4_stub_scalar"),
                "sibling get_pending_registrations must serve archived scalar; \
                 got {scalar_names:?}"
            );
            assert!(
                table_names.iter().any(|n| n == "fu4_stub_table"),
                "sibling get_pending_registrations must serve archived table; \
                 got {table_names:?}"
            );
            assert!(
                agg_names.iter().any(|n| n == "fu4_stub_agg"),
                "sibling get_pending_registrations must serve archived aggregate; \
                 got {agg_names:?}"
            );
        }

        // Regression guard: on the sibling read path, the shared manager's
        // `deferred_registrations` must NOT have been drained a second time.
        // The primary drain above already emptied it, so it should still be
        // empty here — and importantly, this proves the sibling read didn't
        // touch the manager mutex to drain (it read from the archive Arc).
        {
            let mgr = primary_mgr.lock().unwrap();
            assert!(
                mgr.deferred_registrations.scalars.is_empty(),
                "sibling read path must not re-drain manager (deferred_registrations)"
            );
        }
        Ok(())
    }

    // Phase 4 follow-ups:
    //   * [ADDRESSED — FU4 2026-07-27] End-to-end extension-scalar dispatch on
    //     the sibling ALSO requires that the sibling's DuckDB catalog carry
    //     the extension-registered UDFs. The primary's post-LOAD
    //     `get_pending_registrations` drains from the shared manager and
    //     lands the UDFs on the PRIMARY's Database only; the sibling's
    //     Database sees an empty function catalog unless the same
    //     registrations are replayed against it.
    //     FU4 introduces a shared `Arc<Mutex<PendingRegistrationsData>>`
    //     archive on `SiblingState::replay_archive`. The primary's
    //     `impl core_extension_hooks::Host::get_pending_registrations`
    //     appends a snapshot of every drained batch into the archive; the
    //     sibling's `impl core_extension_hooks::Host::get_pending_registrations`
    //     (recognised via `CoreStoreState::is_sibling`) serves the archive
    //     verbatim without touching the shared manager mutex — dodging the
    //     Risk-5 deadlock below. `sibling_ensure_slot` issues one
    //     idempotent `LOAD <name>` on the freshly-opened sibling connection
    //     (using an extension name recorded on
    //     `SiblingState::loaded_extensions`), which prompts the sibling's
    //     wasm shim to pull the archive and register every accumulated
    //     scalar / table / aggregate / macro through the C API in a single
    //     shot. See `phase4_fu4_sibling_replay_archive_populated_from_primary_drain`
    //     (unit test) + `test_phase4_sibling_ext_nested_exec` (E2E lock-in).
    //   * Deadlock boundary (ADR Amendment A5 Risk 5): when the primary is
    //     mid-callback (its `CoreStoreState::call_scalar` holds the manager
    //     mutex — `crates/ducklink-host/src/lib.rs:373`) and the sibling's
    //     dispatch path tries to lock the same mutex, the shared thread
    //     self-deadlocks. FU4's `get_pending_registrations` path avoids the
    //     boundary (archive lives on its own Mutex), but every OTHER shared
    //     `callback-dispatch` host trait still hits the manager mutex. Safe
    //     for pure-DML sibling SQL (no callback → no lock); unsafe if the
    //     sibling SQL invokes any extension scalar. A future §8.5-style
    //     `resolve_dispatch_target` refactor drops the manager mutex before
    //     entering the extension instance, closing the deadlock window.

    // ------------------------------------------------------------------
    // nested-exec Direction-1 §8.5: ExtensionManager mutex reentrancy tests.
    // ------------------------------------------------------------------
    //
    // The old `impl core_callback_dispatch::Host for CoreStoreState` bodies
    // held the `Arc<Mutex<ExtensionManager>>` guard for the full duration of
    // `manager.dispatch_scalar(...)` (which recurses into wasm via
    // `instance.dispatch_scalar`). A nested SQL statement issued from that
    // callback via `primary_nested_exec` -> `guest.call_execute` -> another
    // scalar callback would then try to reacquire the SAME mutex on the SAME
    // thread and self-deadlock.
    //
    // The fix (§8.5): the callback trait impls call
    // `ExtensionManager::resolve_dispatch_target(...)` under a SCOPED lock,
    // drop the guard, then lock the returned `Arc<Mutex<ExtensionInstance>>`
    // and dispatch. The nested callback's own lock attempt therefore finds
    // the manager mutex FREE and proceeds without deadlocking.
    //
    // The tests below prove the pattern at the manager/instance level
    // WITHOUT wasm. `nested_exec_recursion` (the wasm-driven end-to-end
    // repro) is out of scope for this refactor and is called out as a
    // follow-up in §8.5.

    /// Simulator for the fixed callback trait impl: acquires the manager
    /// mutex briefly to "resolve" (as `resolve_dispatch_target` does), drops
    /// the guard, and then runs `dispatch_body` in the released state.
    fn simulate_release_and_reacquire<T>(
        mgr: &Arc<Mutex<ExtensionManager>>,
        dispatch_body: impl FnOnce() -> T,
    ) -> T {
        let _resolved = {
            let guard = mgr.lock().unwrap_or_else(|e| e.into_inner());
            // Model `resolve_dispatch_target`'s access: read something
            // through the guard so the compiler proves it's held here.
            guard.extensions.len()
        }; // <- guard dropped here BEFORE dispatch_body runs
        dispatch_body()
    }

    /// The success criterion of `docs/nested-exec-direction-1-plan.md` §8.5:
    /// the fixed dispatch pattern must permit a callback that itself invokes
    /// a nested-exec chain that fires MORE callbacks, without deadlocking on
    /// the manager mutex.
    ///
    /// Structure:
    ///   outer_dispatch -> nested_dispatch -> inner_dispatch
    /// Each layer follows the release-and-reacquire pattern; each layer's
    /// "resolve" phase locks + drops the same `Arc<Mutex<ExtensionManager>>`.
    /// The old (pre-fix) pattern would hang the second frame because the
    /// first frame would still be holding the guard when it recursed. This
    /// test proves the manager is unlocked when the dispatch body runs and
    /// therefore reacquirable on the same thread.
    #[test]
    fn extension_manager_mutex_reentry_via_release_and_reacquire() {
        let engine = build_engine().expect("engine");
        let mgr = Arc::new(Mutex::new(ExtensionManager::new(engine)));
        let mgr_inner = mgr.clone();
        let mgr_innermost = mgr.clone();

        // Guard against test-suite hang: if a regression re-introduces the
        // deadlock, the timeout thread panics the test process instead of
        // hanging the whole test run. Kept generous (~5s) so it never fires
        // on healthy runs regardless of CI load.
        let (done_tx, done_rx) = std::sync::mpsc::channel::<()>();
        let watchdog = std::thread::spawn(move || {
            match done_rx.recv_timeout(std::time::Duration::from_secs(5)) {
                Ok(()) => {}
                Err(_) => panic!(
                    "extension_manager_mutex_reentry_via_release_and_reacquire deadlocked \
                     (>=5s) — a regression in the callback trait impls likely re-holds \
                     the manager mutex across the dispatch call"
                ),
            }
        });

        let final_value = simulate_release_and_reacquire(&mgr, move || {
            // Outer "dispatch" body — models `instance.dispatch_scalar`
            // running wasm which fires another callback.
            simulate_release_and_reacquire(&mgr_inner, move || {
                // Nested "dispatch" body — one more level, mimicking a
                // deeper recursion via `primary_nested_exec`.
                simulate_release_and_reacquire(&mgr_innermost, || 42u32)
            })
        });

        assert_eq!(final_value, 42);
        // Signal the watchdog that the nested chain returned before the
        // timeout. Ignored if the watchdog already fired (impossible on
        // pass) or was dropped.
        let _ = done_tx.send(());
        watchdog.join().expect("watchdog thread should complete");
    }

    /// Companion to the previous test: prove the OLD pattern (hold the
    /// guard ACROSS the dispatch body) would in fact deadlock — via the
    /// same non-reentrancy `wall1_std_mutex_is_non_reentrant` demonstrates
    /// in `tests/reentrancy_poc.rs`, but applied here to the ACTUAL
    /// `Arc<Mutex<ExtensionManager>>` wrapper shape. Uses `try_lock` so
    /// the test cannot hang — a `WouldBlock` verdict is what proves the
    /// old pattern would have deadlocked on a blocking `lock()`.
    #[test]
    fn extension_manager_mutex_would_deadlock_if_lock_held_across_dispatch() {
        let engine = build_engine().expect("engine");
        let mgr = Arc::new(Mutex::new(ExtensionManager::new(engine)));

        // Simulate the OLD (broken) callback trait impl pattern: acquire
        // the guard and hold it across the "dispatch body".
        let outer_guard = mgr.lock().expect("outer lock");
        let _ = outer_guard.extensions.len(); // ensure we actually hold it
        // While the guard is held, a same-thread `try_lock` MUST fail with
        // `WouldBlock` — same non-reentrancy failure a blocking `lock()`
        // would encounter as a deadlock.
        match mgr.try_lock() {
            Err(std::sync::TryLockError::WouldBlock) => {}
            Err(std::sync::TryLockError::Poisoned(_)) => {
                panic!("manager mutex was poisoned; unexpected")
            }
            Ok(_) => panic!(
                "std::sync::Mutex unexpectedly permitted same-thread re-entry — \
                 wall #1 (non-reentrancy) has changed, so the release-and-reacquire \
                 fix is no longer needed"
            ),
        }
        drop(outer_guard);
    }

    /// Without `PrimaryReentryGuard` installed, `CoreServices::nested_exec`
    /// falls back to the shipped (b.1) sibling — every currently-passing
    /// sibling test above depends on this fallback. Assert the guard is
    /// clear by default so a leaked TLS across tests would surface here.
    #[test]
    fn nested_exec_direction1_reentry_tls_defaults_to_none() {
        let observed = PRIMARY_STORE_REENTRY.with(|slot| slot.get());
        assert!(
            observed.is_none(),
            "PRIMARY_STORE_REENTRY must default to None outside a HostState::execute frame; \
             got Some, likely leaked by a previous test's PrimaryReentryGuard"
        );
    }

    #[test]
    fn is_extension_related_error_flags_missing_function_shapes() {
        // Positive: DuckDB's dominant missing-function shape.
        assert!(is_extension_related_error(
            "Catalog Error: Scalar Function with name aba_validate does not exist!"
        ));
        assert!(is_extension_related_error(
            "Catalog Error: Table Function with name unknown_tf does not exist!"
        ));
        assert!(is_extension_related_error(
            "Missing Extension Error: 'httpfs' is required"
        ));
        assert!(is_extension_related_error(
            "extension 'spatial' is not loaded"
        ));
        assert!(is_extension_related_error(
            "Binder Error: No function matches the given name"
        ));
        // Negative: unrelated failures pass through unchanged.
        assert!(!is_extension_related_error(
            "Parser Error: syntax error at or near \"SELEKT\""
        ));
        assert!(!is_extension_related_error(
            "Catalog Error: Table with name t does not exist"
        )); // table, not function
        assert!(!is_extension_related_error(
            "IO Error: cannot open file"
        ));
    }

    #[test]
    fn sanitize_sibling_open_path_normalizes_memory_marker_to_none() {
        assert_eq!(sanitize_sibling_open_path(None), None);
        assert_eq!(sanitize_sibling_open_path(Some(":memory:")), None);
        assert_eq!(sanitize_sibling_open_path(Some("")), None);
        assert_eq!(sanitize_sibling_open_path(Some("   ")), None);
        assert_eq!(
            sanitize_sibling_open_path(Some("/tmp/foo.duckdb")),
            Some("/tmp/foo.duckdb".to_string())
        );
    }
}
fn resolve_preopens_with_default(preopens: &[(&Path, &str)]) -> Result<Vec<(PathBuf, String)>> {
    let mut merged = Vec::with_capacity(preopens.len() + 3);
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
    // Additive: preopen the absolute cache-root paths that the `cache`
    // extension (extensions/cache-component/src/lib.rs `cache_root()`) resolves
    // from DUCKLINK_LOCAL_CACHE / DUCKLINK_GLOBAL_CACHE. Populates the
    // in-process WasiCtx (`build_wasi_ctx_*`) and — more importantly — the
    // extension loader's per-extension WasiCtx (see the same helper reused
    // where extensions are instantiated). Without it the wasm side fails at
    // `cache: creating <abs>/objects: No such file or directory` because WASI
    // can only resolve an absolute path via a preopen whose guest name matches
    // that same absolute path.
    for (host, guest) in cache_env_preopens() {
        if merged.iter().any(|(existing, _)| existing == &host) {
            continue;
        }
        merged.push((host, guest));
    }
    Ok(merged)
}

/// Absolute cache-root preopens derived from DUCKLINK_LOCAL_CACHE /
/// DUCKLINK_GLOBAL_CACHE. Guest name == absolute host path so
/// `<abs>/objects`, `<abs>/locks`, `<abs>/tmp`, `<abs>/metadata.db` inside the
/// wasm resolve straightforwardly through the matching preopen. Skips cleanly
/// (logs to stderr, does not fail the process) when a var is set to something
/// invalid or non-materialisable on disk — the extension surfaces its own
/// error rather than every host command going down.
fn cache_env_preopens() -> Vec<(PathBuf, String)> {
    let mut out: Vec<(PathBuf, String)> = Vec::new();
    for var in ["DUCKLINK_LOCAL_CACHE", "DUCKLINK_GLOBAL_CACHE"] {
        let Ok(raw) = std::env::var(var) else { continue };
        let raw = raw.trim().to_string();
        if raw.is_empty() {
            continue;
        }
        let path = PathBuf::from(&raw);
        if !path.is_absolute() {
            eprintln!(
                "ducklink-host: ignoring {var}={raw:?}: not an absolute path (WASI preopen requires an absolute path)"
            );
            continue;
        }
        if let Err(e) = std::fs::create_dir_all(&path) {
            eprintln!(
                "ducklink-host: ignoring {var}={}: cannot create directory: {e}",
                path.display()
            );
            continue;
        }
        if out.iter().any(|(p, _)| p == &path) {
            continue;
        }
        let guest = path.to_string_lossy().into_owned();
        out.push((path, guest));
    }
    out
}

/// Attach absolute cache-root preopens (`cache_env_preopens()`) to a
/// `WasiCtxBuilder`. Used at the extension-loader's per-extension WasiCtx
/// construction site, where preopens are built inline rather than through
/// `build_wasi_ctx_*` / `resolve_preopens_with_default`. Missing/invalid
/// vars are already logged inside `cache_env_preopens()`.
fn attach_cache_env_preopens(builder: &mut WasiCtxBuilder) {
    for (host, guest) in cache_env_preopens() {
        if let Err(e) =
            builder.preopened_dir(&host, &guest, DirPerms::all(), FilePerms::all())
        {
            eprintln!(
                "ducklink-host: failed to preopen cache root {}: {e}",
                host.display()
            );
        }
    }
}

/// Grant the sqlitewasm extension filesystem preopens so it can open the
/// ATTACH DSN through wasivfs (sqlite-lib SPI) instead of receiving bytes
/// pre-read from the host. Scoped to the sqlitewasm extension by name so
/// unrelated extensions don't inherit fs reach they didn't declare.
///
/// Two preopens are wired:
///
/// 1. cwd (host = std::env::current_dir(), guest = ".") — the ATTACH tests
///    (and the CLI's typical usage) hand relative DSNs like `foo.db` or
///    `target/.tmpXXX/bar.db`; a cwd preopen resolves them directly.
///
/// 2. DUCKLINK_SQLITEWASM_DATADIR (absolute, guest name == abspath) — for
///    DBs outside cwd. Mirrors the DUCKLINK_LOCAL_CACHE shape so a wasivfs
///    open of `<abs>/…` resolves through a matching preopen. Invalid /
///    non-materialisable settings log-and-skip rather than aborting load.
///
/// DSNs that fall outside both preopens surface as a wasivfs open error at
/// ATTACH time. No dynamic preopens — wasmtime preopens are static per
/// WasiCtx — so an operator with DBs outside cwd sets
/// DUCKLINK_SQLITEWASM_DATADIR before LOAD sqlitewasm.
fn attach_sqlitewasm_preopens(builder: &mut WasiCtxBuilder, extension_name: &str) {
    if extension_name != "sqlitewasm" {
        return;
    }
    match std::env::current_dir() {
        Ok(cwd) => {
            if let Err(e) =
                builder.preopened_dir(&cwd, ".", DirPerms::all(), FilePerms::all())
            {
                eprintln!(
                    "ducklink-host: failed to preopen cwd for sqlitewasm ({}): {e}",
                    cwd.display()
                );
            }
        }
        Err(e) => {
            eprintln!("ducklink-host: sqlitewasm preopens: cannot read cwd ({e})");
        }
    }
    if let Ok(raw) = std::env::var("DUCKLINK_SQLITEWASM_DATADIR") {
        let raw = raw.trim().to_string();
        if raw.is_empty() {
            return;
        }
        let path = PathBuf::from(&raw);
        if !path.is_absolute() {
            eprintln!(
                "ducklink-host: ignoring DUCKLINK_SQLITEWASM_DATADIR={raw:?}: \
                 not an absolute path (WASI preopen requires an absolute path)"
            );
            return;
        }
        if let Err(e) = std::fs::create_dir_all(&path) {
            eprintln!(
                "ducklink-host: ignoring DUCKLINK_SQLITEWASM_DATADIR={}: \
                 cannot create directory: {e}",
                path.display()
            );
            return;
        }
        let guest = path.to_string_lossy().into_owned();
        if let Err(e) =
            builder.preopened_dir(&path, &guest, DirPerms::all(), FilePerms::all())
        {
            eprintln!(
                "ducklink-host: failed to preopen sqlitewasm datadir {}: {e}",
                path.display()
            );
        }
    }
}
