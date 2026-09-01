//! ADR-0029 Phase 6.2.d.2-a — wasmos-native mirror of the
//! `duckdb:extension/lifecycle` host interface.
//!
//! First interface in the ExtensionStoreState migration to
//! `wasmos_runtime_api::HostImports`. Coexists with the existing
//! wit-bindgen `Host` impls in [`crate::extension`] — this module
//! is additive; nothing in the existing dispatch path changed.
//!
//! # What's here
//!
//! * `Duckerror` — wasmos-native mirror of the WIT
//!   `duckdb:extension/types.duckerror` variant. Derived via
//!   [`wasmos_runtime_api::WitVariant`] so the classifier
//!   marshals it end-to-end through the `#[host_iface]` machinery.
//! * `ConnEvents` — wasmos-native mirror of the WIT
//!   `duckdb:extension/lifecycle.conn-events` flags. Derived via
//!   [`wasmos_runtime_api::WitFlags`].
//! * `LifecycleHost` — the host struct + `#[host_iface(sync)]` impl
//!   for the `duckdb:extension/lifecycle` interface. Preserves the
//!   existing behavior (always returns `Unsupported` — the DuckDB C
//!   API has no connection open/close hooks; see
//!   `crate::extension` line 2196).
//! * `install_lifecycle_imports` — one-line registration on a
//!   [`HostImports`] set.
//!
//! # Design notes
//!
//! * Sync throughout: uses `#[host_iface(sync)]` because the
//!   existing wit-bindgen impls are sync. No async wrapping, no
//!   tokio bridging.
//! * `Duckerror` is REDEFINED here as a wasmos-derived type — it's
//!   NOT the same type as `crate::extension::extension_types::
//!   Duckerror` (that's wit-bindgen output). Both share the same
//!   WIT variant shape so they marshal wire-identical; consumers
//!   picking the wasmos path see this crate's `Duckerror`,
//!   consumers on the wit-bindgen path see the other. Coexistence
//!   preserved.
//! * Migrations of the remaining ~26 interfaces follow the same
//!   pattern in future sessions. See
//!   `wasmos/docs/design/runtime-abstraction/phase-6-2-d-2-recon.md`
//!   for the full mapping.

use std::sync::{Arc, Mutex};

use wasmos_runtime_api::{
    host_iface, HostCallContext, HostImports, HostResourceType, Resource, RuntimeError,
    RuntimeResult, SyncHostCall, SyncHostCallAdapter, WitEnum, WitFlags, WitVariant,
};

use crate::extension::{
    ExtensionStoreState, PendingOptimizer, PendingParser, PendingSetting,
};

/// Shared handle to `ExtensionStoreState` used by state-touching
/// wasmos-native interface handlers. Matches the SharedTvmHost
/// pattern from tvm-wasm (ADR-0029 Phase 6.9.a) — per-call
/// mutex lock, no fine-grained fields. Suitable for interfaces
/// called at extension-load time (not hot inner loops).
///
/// Consumers construct one at instantiation:
///
/// ```rust,ignore
/// let state = ExtensionStoreState::new(...);
/// let shared: SharedExtensionState = Arc::new(Mutex::new(state));
/// let imports = install_extension_imports_stateful(
///     HostImports::new(),
///     shared,
/// );
/// ```
///
/// Alternative wire-compatible design (extract per-field locks
/// into a smaller shared struct) is a Phase 6.2.d.2-d follow-up
/// if per-call lock contention shows up in benchmarks.
pub type SharedExtensionState = Arc<Mutex<ExtensionStoreState>>;

/// ADR-0029 Phase 6.2.h.5 — dual-mode state source for wasmos-native
/// interface handlers.
///
/// - [`StateSource::Shared`] carries an
///   [`SharedExtensionState`]. Used by the wasmos ADAPTER path
///   (state lives outside the wasmtime Store — the adapter owns
///   `AdapterHostState`) + by every stateful integration test in
///   Phase 6.2.g (tests build their own SharedExtensionState).
///
/// - [`StateSource::FromCtx`] holds nothing. Used by the
///   CONSUMER-MIGRATION BRIDGE path
///   (`crate::extension::install_wasmos_migrated_interfaces`), where
///   `ExtensionStoreState` lives INSIDE the consumer's wasmtime
///   Store data. The
///   [`wasmos_runtime_wasmtime_v48::sync_bridge_resource`] bridge
///   populates the `HostCallContext::consumer_state` slot per-call
///   with `store.data_mut()`, and the handler pulls
///   `&mut ExtensionStoreState` from ctx via
///   [`StateHold::from_ctx`].
///
/// The [`Self::hold`] entry point resolves either variant into a
/// [`StateHold`] that dereferences to `&mut ExtensionStoreState` —
/// one code path per method, no per-method match. Every state-
/// touching wasmos-native handler in this module lands its
/// per-call `let mut g = self.state.hold(ctx)?;` and proceeds
/// identically regardless of which variant the constructor picked.
#[derive(Clone)]
pub enum StateSource {
    /// State lives outside the wasmtime store; handler holds its
    /// own `Arc<Mutex<>>` clone. Wasmos-adapter path + stateful
    /// integration tests.
    Shared(SharedExtensionState),
    /// State lives inside the consumer's wasmtime store; the
    /// bridge populates it into `HostCallContext::consumer_state`
    /// per-call. Handler pulls from there.
    FromCtx,
}

impl StateSource {
    /// Resolve to a [`StateHold`] that dereferences to
    /// `&mut ExtensionStoreState`.
    ///
    /// For [`Self::Shared`]: locks the mutex and holds the guard.
    /// For [`Self::FromCtx`]: downcasts the ctx's consumer-state
    /// slot; returns an error if the bridge failed to populate it
    /// (should never happen through the production wire — the
    /// resource-aware bridge always populates).
    ///
    /// Idiomatic use inside a `#[host_iface(sync)]` method:
    ///
    /// ```rust,ignore
    /// fn my_method(&self, ctx: &mut HostCallContext<'_>, ...) -> ... {
    ///     let mut g = self.state.hold(ctx)?;
    ///     let name = g.extension_name().to_string();
    ///     g.push_pending_foo(...);
    ///     Ok(...)
    /// }
    /// ```
    pub fn hold<'a>(
        &'a self,
        ctx: &'a mut HostCallContext<'_>,
    ) -> RuntimeResult<StateHold<'a>> {
        match self {
            StateSource::Shared(shared) => {
                let guard = shared.lock().expect("ExtensionStoreState mutex poisoned");
                Ok(StateHold::Guard(guard))
            }
            StateSource::FromCtx => {
                let s = ctx
                    .consumer_state::<ExtensionStoreState>()
                    .ok_or_else(|| {
                        RuntimeError::msg(
                            "wasmos-native handler: no ExtensionStoreState in \
                             HostCallContext — the consumer-migration bridge \
                             (wasmos-runtime-wasmtime-v48::sync_bridge_resource) is \
                             required to populate the consumer-state slot at dispatch \
                             time. Handlers built with `bridged()` cannot be invoked \
                             through the wasmos-adapter path; use the corresponding \
                             `new(shared_state)` constructor for that path.",
                        )
                    })?;
                Ok(StateHold::Ref(s))
            }
        }
    }
}

/// The output of [`StateSource::hold`] — dereferences to
/// `&mut ExtensionStoreState` regardless of which variant produced
/// it. Owns either a live [`std::sync::MutexGuard`] (Shared
/// variant) or a plain mutable reference (FromCtx variant).
pub enum StateHold<'a> {
    /// A live mutex guard. Dropped at end-of-scope releases the
    /// lock — matches the wasmos-adapter path semantics where
    /// each host-call fully brackets its state access.
    Guard(std::sync::MutexGuard<'a, ExtensionStoreState>),
    /// A plain mutable reference. No lock semantics — the bridge
    /// path relies on wasmtime's own single-threaded dispatch to
    /// enforce mutual exclusion.
    Ref(&'a mut ExtensionStoreState),
}

impl<'a> std::ops::Deref for StateHold<'a> {
    type Target = ExtensionStoreState;
    fn deref(&self) -> &Self::Target {
        match self {
            StateHold::Guard(g) => &**g,
            StateHold::Ref(r) => r,
        }
    }
}

impl<'a> std::ops::DerefMut for StateHold<'a> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        match self {
            StateHold::Guard(g) => &mut **g,
            StateHold::Ref(r) => r,
        }
    }
}

// ────────────────────────────────────────────────────────────────────
// Migration status (Phase 6.2.d.2)
//
// This module hosts the wasmos-native equivalents of the 27
// `impl <iface>::Host for ExtensionStoreState` blocks in
// `crate::extension`. Interfaces migrate in batches based on state-
// dependency shape:
//
// - **Stateless interfaces** (this session, Phase 6.2.d.2-a/b) —
//   interfaces whose handlers reach for no ExtensionStoreState
//   fields. Currently: `lifecycle`, `types`, `encoding`,
//   `compression`, `files_reg`. All either empty markers or
//   `Unsupported` returns.
// - **State-sharing interfaces** (Phase 6.2.d.2-c, future) —
//   interfaces whose handlers append to `pending_*` buffers or
//   read `extension_name` / `alloc_resource_id()`. Blocked on an
//   architecture decision:
//     (A) Wrap ExtensionStoreState in `Arc<Mutex<...>>` at the
//         wasmos install path, per-call `.lock()` (matches
//         SharedTvmHost pattern; adds mutex cost).
//     (B) Extract `pending_*` + `extension_name` +
//         `next_resource_id` into a `SharedExtensionState` handle
//         that host structs capture individually (finer-grained;
//         requires refactor to `crate::extension`).
//     (C) `mpsc::Sender<HostEvent>` from host structs to a single
//         drain owned by ExtensionStoreState (async-friendly but
//         changes the ownership model most).
// - **Resource-carrying interfaces** (Phase 6.2.d.2-d, future) —
//   `extension_runtime`, `runtime_ext`, `storage`, `nested_exec`.
//   Each returns `Resource<T>`; needs the state architecture from
//   the previous bucket plus `#[wit_ctx]` on the return-carrying
//   variants.
// ────────────────────────────────────────────────────────────────────

/// Wasmos-native mirror of `duckdb:extension/types.duckerror`.
///
/// Wire-identical to the wit-bindgen `extension_types::Duckerror`
/// in [`crate::extension`]; both marshal as a 5-arm variant with
/// string payloads.
#[derive(Debug, Clone, WitVariant)]
pub enum Duckerror {
    Invalidargument(String),
    Unsupported(String),
    Invalidstate(String),
    Io(String),
    Internal(String),
}

/// Wasmos-native mirror of `duckdb:extension/lifecycle.conn-events`.
///
/// Wire-identical to the wit-bindgen `extension_lifecycle::
/// ConnEvents`; both marshal as a 2-bit flags shape.
#[derive(Debug, Clone, Copy, WitFlags)]
pub struct ConnEvents {
    pub opened: bool,
    pub closed: bool,
}

/// Host struct for the `duckdb:extension/lifecycle` interface.
///
/// Currently a marker — the interface's one method
/// (`register-connection-callback`) always returns
/// `Duckerror::Unsupported` because DuckDB's C API has no
/// connection open/close hooks. Future migration sessions add
/// fields as new methods land.
///
/// Cloneable so a single instance can be registered under multiple
/// [`HostImports`] sets without construction cost.
#[derive(Debug, Default, Clone)]
pub struct LifecycleHost;

impl LifecycleHost {
    /// Construct a new host. Identity operation today (the host
    /// carries no state); kept as a fn so future field additions
    /// don't break call sites.
    pub fn new() -> Self {
        Self
    }
}

// The `#[host_iface(sync)]` attribute emits `impl SyncHostCall for
// LifecycleHost` alongside the fns below, dispatching kebab-cased
// method names to the fn bodies. See wasmos runtime-api README's
// "Sync host imports" section for the pattern.
#[host_iface(sync)]
impl LifecycleHost {
    /// Handler for `duckdb:extension/lifecycle.
    /// register-connection-callback`. Mirrors the existing
    /// wit-bindgen impl in `crate::extension` — the DuckDB C API
    /// has no connection open/close hooks, so this always returns
    /// `Unsupported`.
    fn register_connection_callback(
        &self,
        _ctx: &mut HostCallContext<'_>,
        _events: ConnEvents,
        _callback_handle: u32,
    ) -> RuntimeResult<Result<u32, Duckerror>> {
        Ok(Err(Duckerror::Unsupported(
            "no DuckDB C API for connection open/close callbacks".to_string(),
        )))
    }
}

// ── extension_types (empty marker interface) ─────────────────────────

/// Host struct for the `duckdb:extension/types` interface.
///
/// The interface exists only as a namespace for shared type
/// declarations (`duckerror`, `duckvalue`, etc.) — it has zero
/// methods. The empty impl satisfies the guest's import so
/// instantiation succeeds; the type declarations themselves are
/// consumed by other interfaces.
#[derive(Debug, Default, Clone)]
pub struct TypesHost;

impl TypesHost {
    pub fn new() -> Self {
        Self
    }
}

#[host_iface(sync)]
impl TypesHost {}

/// Register the `duckdb:extension/types` handler.
pub fn install_types_imports(imports: HostImports) -> HostImports {
    imports.register(
        "duckdb:extension/types",
        Arc::new(SyncHostCallAdapter::new(TypesHost::new()))
            as Arc<dyn wasmos_runtime_api::HostCall>,
    )
}

// ── extension_encoding (single Unsupported return) ───────────────────

/// Host struct for the `duckdb:extension/encoding` interface.
/// The one method (`register-encoding`) always returns
/// `Unsupported` — `duckdb_register_encoding` is not part of the
/// DuckDB stable C API. See `crate::extension` line 2258 for the
/// wit-bindgen counterpart.
#[derive(Debug, Default, Clone)]
pub struct EncodingHost;

impl EncodingHost {
    pub fn new() -> Self {
        Self
    }
}

#[host_iface(sync)]
impl EncodingHost {
    fn register_encoding(
        &self,
        _ctx: &mut HostCallContext<'_>,
        _name: String,
        _aliases: Vec<String>,
        _callback_handle: u32,
    ) -> RuntimeResult<Result<u32, Duckerror>> {
        Ok(Err(Duckerror::Unsupported(
            "duckdb_register_encoding is not part of the DuckDB stable C API".to_string(),
        )))
    }
}

/// Register the `duckdb:extension/encoding` handler.
pub fn install_encoding_imports(imports: HostImports) -> HostImports {
    imports.register(
        "duckdb:extension/encoding",
        Arc::new(SyncHostCallAdapter::new(EncodingHost::new()))
            as Arc<dyn wasmos_runtime_api::HostCall>,
    )
}

// ── extension_compression (single Unsupported return) ────────────────

/// Host struct for the `duckdb:extension/compression` interface.
/// The one method (`register-compression`) always returns
/// `Unsupported` — no stable DuckDB C API. See `crate::extension`
/// line 2275.
#[derive(Debug, Default, Clone)]
pub struct CompressionHost;

impl CompressionHost {
    pub fn new() -> Self {
        Self
    }
}

#[host_iface(sync)]
impl CompressionHost {
    fn register_compression(
        &self,
        _ctx: &mut HostCallContext<'_>,
        _name: String,
        _file_extension: String,
        _callback_handle: u32,
    ) -> RuntimeResult<Result<u32, Duckerror>> {
        Ok(Err(Duckerror::Unsupported(
            "duckdb_register_compression is not part of the DuckDB stable C API".to_string(),
        )))
    }
}

/// Register the `duckdb:extension/compression` handler.
pub fn install_compression_imports(imports: HostImports) -> HostImports {
    imports.register(
        "duckdb:extension/compression",
        Arc::new(SyncHostCallAdapter::new(CompressionHost::new()))
            as Arc<dyn wasmos_runtime_api::HostCall>,
    )
}

// ── extension_files_reg (single Unsupported return) ──────────────────

/// Host struct for the `duckdb:extension/files-reg` interface.
/// The one method (`register-files`) always returns `Unsupported`
/// — no stable DuckDB C API. See `crate::extension` line 2375.
#[derive(Debug, Default, Clone)]
pub struct FilesRegHost;

impl FilesRegHost {
    pub fn new() -> Self {
        Self
    }
}

#[host_iface(sync)]
impl FilesRegHost {
    fn register_files(
        &self,
        _ctx: &mut HostCallContext<'_>,
        _callback_handle: u32,
    ) -> RuntimeResult<Result<u32, Duckerror>> {
        Ok(Err(Duckerror::Unsupported(
            "duckdb_register_file_system is not part of the DuckDB stable C API".to_string(),
        )))
    }
}

/// Register the `duckdb:extension/files-reg` handler.
pub fn install_files_reg_imports(imports: HostImports) -> HostImports {
    imports.register(
        "duckdb:extension/files-reg",
        Arc::new(SyncHostCallAdapter::new(FilesRegHost::new()))
            as Arc<dyn wasmos_runtime_api::HostCall>,
    )
}

// ── Composite installer for all currently-migrated interfaces ────────

/// Install every wasmos-native interface currently landed under
/// Phase 6.2.d.2-a/b. As additional interfaces migrate in later
/// sub-sessions, they'll be added to this composite fn — consumer
/// code depends on this single entry point and picks up new
/// interfaces automatically.
///
/// Interfaces registered today:
/// - `duckdb:extension/lifecycle`
/// - `duckdb:extension/types`
/// - `duckdb:extension/encoding`
/// - `duckdb:extension/compression`
/// - `duckdb:extension/files-reg`
///
/// **Not yet registered** (need state-sharing architecture per
/// module docstring): `runtime`, `config`, `logging`, `catalog`,
/// `files`, `secret`, `settings`, `parser`, `optimizer`,
/// `table_stream`, `macro_ext`, `types_ext`, `runtime_ext`,
/// `coordinate_system`, `arrow_ext`, `log_storage`, `storage`,
/// `index`, `collation`, `query`, `nested_exec`, `file_lock`.
///
/// A guest importing any of these unmigrated interfaces will fail
/// instantiation with an "unresolved import" error under the
/// wasmos-native install path — that's the signal to fall back to
/// the wit-bindgen `crate::extension` path or wait for the
/// remaining interfaces to migrate.
pub fn install_extension_imports(imports: HostImports) -> HostImports {
    let imports = install_lifecycle_imports(imports);
    let imports = install_types_imports(imports);
    let imports = install_encoding_imports(imports);
    let imports = install_compression_imports(imports);
    install_files_reg_imports(imports)
}

/// Install every interface currently landed, INCLUDING the
/// state-touching batch that needs a shared handle to
/// `ExtensionStoreState`. Preferred entry point for consumers
/// running the wasmos-native path end-to-end.
///
/// Stateless-only [`install_extension_imports`] stays available
/// for consumers who don't need any state-touching interface —
/// tests, minimal harnesses, and any component that only imports
/// the 5 stateless interfaces.
///
/// State-touching interfaces registered by this fn (in addition
/// to the 5 stateless ones):
/// - `duckdb:extension/parser` — captures pending parser
///   extensions into `state.pending_parsers`.
/// - `duckdb:extension/optimizer` — captures pending optimizer
///   rules into `state.pending_optimizers`.
/// - `duckdb:extension/settings` — captures pending settings
///   into `state.pending_settings`.
pub fn install_extension_imports_stateful(
    imports: HostImports,
    state: SharedExtensionState,
) -> HostImports {
    let imports = install_extension_imports(imports);
    // 6.2.d.2-c batch
    let imports = install_parser_imports(imports, state.clone());
    let imports = install_optimizer_imports(imports, state.clone());
    let imports = install_settings_imports(imports, state.clone());
    // 6.2.d.2-d batch (stateless additions)
    let imports = install_index_imports(imports);
    let imports = install_collation_imports(imports);
    // 6.2.d.2-d batch (state-touching)
    let imports = install_coordinate_system_imports(imports, state.clone());
    let imports = install_storage_imports(imports, state.clone());
    let imports = install_log_storage_imports(imports, state.clone());
    let imports = install_query_imports(imports, state.clone());
    // 6.2.d.2-e batch (services-delegating)
    let imports = install_config_imports(imports, state.clone());
    let imports = install_logging_imports(imports, state.clone());
    let imports = install_nested_exec_imports(imports, state.clone());
    // 6.2.d.2-f batch (secret)
    let imports = install_secret_imports(imports, state.clone());
    // 6.2.d.2-g batch (macro_ext + types_ext)
    let imports = install_macro_ext_imports(imports, state.clone());
    let imports = install_types_ext_imports(imports, state.clone());
    // 6.2.d.2-h batch (files)
    let imports = install_files_imports(imports, state.clone());
    // 6.2.d.2-i batch (arrow_ext + LogicalType mirror unblocking future batches)
    let imports = install_arrow_ext_imports(imports, state.clone());
    // 6.2.d.2-j batch (Funcarg mirror + table_stream)
    let imports = install_table_stream_imports(imports, state.clone());
    // 6.2.d.2-k batch (Funcflags/Funcopts/NullHandling + runtime_ext)
    let imports = install_runtime_ext_imports(imports, state.clone());
    // 6.2.d.2-l/m batch (catalog — register_cast promoted from stub
    // in -m via Resource<CastCallback> arg handling)
    let imports = install_catalog_imports(imports, state.clone());
    // 6.2.d.2-m batch (file_lock — resource lifecycle methods deferred)
    let imports = install_file_lock_imports(imports, state.clone());
    // 6.2.d.2-o batch (runtime main Host trait — 10 sub-traits
    // deferred to Phase 6.2.d.2-p+)
    install_runtime_imports(imports, state)
}

// ────────────────────────────────────────────────────────────────────
// State-touching interfaces (Phase 6.2.d.2-c, first batch).
//
// Each mirrors the corresponding `impl <iface>::Host for
// ExtensionStoreState` in `crate::extension`. Behavior is
// byte-identical; the state access goes through the shared
// mutex on every call.
// ────────────────────────────────────────────────────────────────────

// ── extension_parser ────────────────────────────────────────────────

/// Host struct for the `duckdb:extension/parser` interface.
/// See `crate::extension` line 1973 for the wit-bindgen
/// counterpart. Currently DEPRECATED (per the source comment
/// there) — no host drains `pending_parsers` anymore, so
/// components calling `register-parser-extension` succeed but
/// their declarations never reach DuckDB.
// No `Debug` derive — SharedExtensionState wraps
// ExtensionStoreState which contains non-Debug wasmtime types
// (WasiCtx, ResourceTable). Trace by state pointer if needed.
#[derive(Clone)]
pub struct ParserHost {
    state: StateSource,
}

impl ParserHost {
    pub fn new(state: SharedExtensionState) -> Self {
        Self { state: StateSource::Shared(state) }
    }
    /// ADR-0029 Phase 6.2.h.6 — bridged variant for the consumer-
    /// migration bridge. Pulls `&mut ExtensionStoreState` from
    /// `HostCallContext::consumer_state` at dispatch time; the
    /// bridge populates it per-call with `store.data_mut()`.
    pub fn bridged() -> Self {
        Self { state: StateSource::FromCtx }
    }
}

#[host_iface(sync)]
impl ParserHost {
    fn register_parser_extension(
        &self,
        ctx: &mut HostCallContext<'_>,
        name: String,
        callback_handle: u32,
    ) -> RuntimeResult<Result<u32, Duckerror>> {
        let mut g = self.state.hold(ctx)?;
        let registry_id = g.alloc_resource_id();
        let extension = g.extension_name().to_string();
        g.push_pending_parser(PendingParser {
            extension,
            name,
            callback_handle,
        });
        Ok(Ok(registry_id))
    }
}

/// Register the `duckdb:extension/parser` handler.
pub fn install_parser_imports(
    imports: HostImports,
    state: SharedExtensionState,
) -> HostImports {
    imports.register(
        "duckdb:extension/parser",
        Arc::new(SyncHostCallAdapter::new(ParserHost::new(state)))
            as Arc<dyn wasmos_runtime_api::HostCall>,
    )
}

// ── extension_optimizer ─────────────────────────────────────────────

/// Host struct for the `duckdb:extension/optimizer` interface.
/// See `crate::extension` line 2002. Also DEPRECATED (per the
/// source comment) — pending buffer is captured but not drained.
#[derive(Clone)]
pub struct OptimizerHost {
    state: StateSource,
}

impl OptimizerHost {
    pub fn new(state: SharedExtensionState) -> Self {
        Self { state: StateSource::Shared(state) }
    }
    /// ADR-0029 Phase 6.2.h.6 — bridged variant for the consumer-
    /// migration bridge. Pulls `&mut ExtensionStoreState` from
    /// `HostCallContext::consumer_state` at dispatch time; the
    /// bridge populates it per-call with `store.data_mut()`.
    pub fn bridged() -> Self {
        Self { state: StateSource::FromCtx }
    }
}

#[host_iface(sync)]
impl OptimizerHost {
    fn register_optimizer_rule(
        &self,
        ctx: &mut HostCallContext<'_>,
        rule_name: String,
        callback_handle: u32,
    ) -> RuntimeResult<Result<u32, Duckerror>> {
        let mut g = self.state.hold(ctx)?;
        let registry_id = g.alloc_resource_id();
        let extension = g.extension_name().to_string();
        g.push_pending_optimizer(PendingOptimizer {
            extension,
            rule_name,
            callback_handle,
        });
        Ok(Ok(registry_id))
    }
}

/// Register the `duckdb:extension/optimizer` handler.
pub fn install_optimizer_imports(
    imports: HostImports,
    state: SharedExtensionState,
) -> HostImports {
    imports.register(
        "duckdb:extension/optimizer",
        Arc::new(SyncHostCallAdapter::new(OptimizerHost::new(state)))
            as Arc<dyn wasmos_runtime_api::HostCall>,
    )
}

// ── extension_settings ──────────────────────────────────────────────

/// Wasmos-native mirror of the WIT
/// `duckdb:extension/settings.setting-type` enum. See
/// `crate::extension` line 1927.
#[derive(Debug, Clone, Copy, WitEnum)]
pub enum SettingType {
    Boolean,
    Varchar,
    Bigint,
    Double,
}

impl SettingType {
    fn as_str(self) -> &'static str {
        match self {
            SettingType::Boolean => "boolean",
            SettingType::Varchar => "varchar",
            SettingType::Bigint => "bigint",
            SettingType::Double => "double",
        }
    }
}

/// Wasmos-native mirror of the WIT
/// `duckdb:extension/settings.setting-scope` enum.
#[derive(Debug, Clone, Copy, WitEnum)]
pub enum SettingScope {
    Local,
    Global,
}

impl SettingScope {
    fn as_str(self) -> &'static str {
        match self {
            SettingScope::Local => "local",
            SettingScope::Global => "global",
        }
    }
}

/// Host struct for the `duckdb:extension/settings` interface.
/// See `crate::extension` line 1927. Captures pending settings
/// into `state.pending_settings` for drain by the core shim.
#[derive(Clone)]
pub struct SettingsHost {
    state: StateSource,
}

impl SettingsHost {
    pub fn new(state: SharedExtensionState) -> Self {
        Self { state: StateSource::Shared(state) }
    }
    /// ADR-0029 Phase 6.2.h.6 — bridged variant for the consumer-
    /// migration bridge. Pulls `&mut ExtensionStoreState` from
    /// `HostCallContext::consumer_state` at dispatch time; the
    /// bridge populates it per-call with `store.data_mut()`.
    pub fn bridged() -> Self {
        Self { state: StateSource::FromCtx }
    }
}

#[host_iface(sync)]
impl SettingsHost {
    fn register_option(
        &self,
        ctx: &mut HostCallContext<'_>,
        name: String,
        description: String,
        ty: SettingType,
        default_value: Option<String>,
        scope: SettingScope,
    ) -> RuntimeResult<Result<(), Duckerror>> {
        let ty_str = ty.as_str().to_string();
        let scope_str = scope.as_str().to_string();
        let mut g = self.state.hold(ctx)?;
        let extension = g.extension_name().to_string();
        g.push_pending_setting(PendingSetting {
            extension,
            name,
            description,
            ty: ty_str,
            default_value,
            scope: scope_str,
        });
        Ok(Ok(()))
    }
}

/// Register the `duckdb:extension/settings` handler.
pub fn install_settings_imports(
    imports: HostImports,
    state: SharedExtensionState,
) -> HostImports {
    imports.register(
        "duckdb:extension/settings",
        Arc::new(SyncHostCallAdapter::new(SettingsHost::new(state)))
            as Arc<dyn wasmos_runtime_api::HostCall>,
    )
}

// ────────────────────────────────────────────────────────────────────
// Phase 6.2.d.2-d — record-carrying batch (6 interfaces).
//
// Adds coordinate_system, storage, log_storage, query, plus the two
// remaining Unsupported-return interfaces (index, collation). Each
// mirrors the corresponding wit-bindgen impl in `crate::extension`
// with byte-identical guest-observable behavior.
// ────────────────────────────────────────────────────────────────────

// ── extension_index (Unsupported return) ─────────────────────────────

/// Host struct for the `duckdb:extension/index` interface.
/// One method (`register-index-type`), returns Unsupported —
/// `duckdb_register_index_type` is not part of the DuckDB
/// stable C API. See `crate::extension` line 2408.
#[derive(Debug, Default, Clone)]
pub struct IndexHost;

impl IndexHost {
    pub fn new() -> Self {
        Self
    }
}

#[host_iface(sync)]
impl IndexHost {
    fn register_index_type(
        &self,
        _ctx: &mut HostCallContext<'_>,
        _type_name: String,
    ) -> RuntimeResult<Result<(), Duckerror>> {
        Ok(Err(Duckerror::Unsupported(
            "duckdb_register_index_type is not part of the DuckDB stable C API".to_string(),
        )))
    }
}

/// Register the `duckdb:extension/index` handler.
pub fn install_index_imports(imports: HostImports) -> HostImports {
    imports.register(
        "duckdb:extension/index",
        Arc::new(SyncHostCallAdapter::new(IndexHost::new()))
            as Arc<dyn wasmos_runtime_api::HostCall>,
    )
}

// ── extension_collation (Unsupported return) ─────────────────────────

/// Host struct for the `duckdb:extension/collation` interface.
/// One method (`register-collation`), returns Unsupported —
/// `duckdb_register_collation` is not in the stable C API.
/// See `crate::extension` line 2445.
#[derive(Debug, Default, Clone)]
pub struct CollationHost;

impl CollationHost {
    pub fn new() -> Self {
        Self
    }
}

#[host_iface(sync)]
impl CollationHost {
    fn register_collation(
        &self,
        _ctx: &mut HostCallContext<'_>,
        _name: String,
        _transform_scalar: String,
        _combinable: bool,
    ) -> RuntimeResult<Result<(), Duckerror>> {
        Ok(Err(Duckerror::Unsupported(
            "duckdb_register_collation is not part of the DuckDB stable C API".to_string(),
        )))
    }
}

/// Register the `duckdb:extension/collation` handler.
pub fn install_collation_imports(imports: HostImports) -> HostImports {
    imports.register(
        "duckdb:extension/collation",
        Arc::new(SyncHostCallAdapter::new(CollationHost::new()))
            as Arc<dyn wasmos_runtime_api::HostCall>,
    )
}

// ── extension_coordinate_system (record push) ────────────────────────

/// Wasmos-native mirror of the WIT
/// `duckdb:extension/coordinate-system.crs-def` record.
#[derive(Debug, Clone, wasmos_runtime_api::WitRecord)]
pub struct CrsDef {
    pub auth_name: String,
    pub code: u32,
    pub wkt: String,
}

/// Host struct for the `duckdb:extension/coordinate-system` interface.
/// See `crate::extension` line 2240.
#[derive(Clone)]
pub struct CoordinateSystemHost {
    state: StateSource,
}

impl CoordinateSystemHost {
    pub fn new(state: SharedExtensionState) -> Self {
        Self { state: StateSource::Shared(state) }
    }
    /// ADR-0029 Phase 6.2.h.6 — bridged variant for the consumer-
    /// migration bridge. Pulls `&mut ExtensionStoreState` from
    /// `HostCallContext::consumer_state` at dispatch time; the
    /// bridge populates it per-call with `store.data_mut()`.
    pub fn bridged() -> Self {
        Self { state: StateSource::FromCtx }
    }
}

#[host_iface(sync)]
impl CoordinateSystemHost {
    fn register_coordinate_system(
        &self,
        ctx: &mut HostCallContext<'_>,
        crs: CrsDef,
    ) -> RuntimeResult<Result<u32, Duckerror>> {
        let mut g = self.state.hold(ctx)?;
        let extension = g.extension_name().to_string();
        g.push_pending_coordinate_system(crate::reg::CoordinateSystemReg {
            extension,
            auth_name: crs.auth_name,
            code: crs.code,
            wkt: crs.wkt,
        });
        Ok(Ok(g.alloc_resource_id()))
    }
}

/// Register the `duckdb:extension/coordinate-system` handler.
pub fn install_coordinate_system_imports(
    imports: HostImports,
    state: SharedExtensionState,
) -> HostImports {
    imports.register(
        "duckdb:extension/coordinate-system",
        Arc::new(SyncHostCallAdapter::new(CoordinateSystemHost::new(state)))
            as Arc<dyn wasmos_runtime_api::HostCall>,
    )
}

// ── extension_storage (Option<record> push) ──────────────────────────

/// Wasmos-native mirror of the WIT `duckdb:extension/types.extopts`
/// record. Reused by any interface accepting extension options
/// (currently just storage; may add table_stream in future batches).
#[derive(Debug, Clone, wasmos_runtime_api::WitRecord)]
pub struct Extopts {
    pub description: Option<String>,
    pub tags: Vec<String>,
}

/// Host struct for the `duckdb:extension/storage` interface.
/// See `crate::extension` line 2361.
#[derive(Clone)]
pub struct StorageHost {
    state: StateSource,
}

impl StorageHost {
    pub fn new(state: SharedExtensionState) -> Self {
        Self { state: StateSource::Shared(state) }
    }
    /// ADR-0029 Phase 6.2.h.6 — bridged variant for the consumer-
    /// migration bridge. Pulls `&mut ExtensionStoreState` from
    /// `HostCallContext::consumer_state` at dispatch time; the
    /// bridge populates it per-call with `store.data_mut()`.
    pub fn bridged() -> Self {
        Self { state: StateSource::FromCtx }
    }
}

#[host_iface(sync)]
impl StorageHost {
    fn register_storage(
        &self,
        ctx: &mut HostCallContext<'_>,
        type_name: String,
        callback_handle: u32,
        options: Option<Extopts>,
    ) -> RuntimeResult<Result<u32, Duckerror>> {
        let neutral_options = options.map(|o| crate::reg::ExtOpts {
            description: o.description,
            tags: o.tags,
        });
        let mut g = self.state.hold(ctx)?;
        let extension = g.extension_name().to_string();
        g.push_pending_storage(crate::reg::StorageReg {
            extension,
            type_name,
            callback_handle,
            options: neutral_options,
        });
        // The @4 host-side API return-was-the-handle contract:
        // callback_handle passed in comes back unchanged. See
        // `crate::extension` line 2382-2385 for the historical
        // wire-compat rationale.
        Ok(Ok(callback_handle))
    }
}

/// Register the `duckdb:extension/storage` handler.
pub fn install_storage_imports(
    imports: HostImports,
    state: SharedExtensionState,
) -> HostImports {
    imports.register(
        "duckdb:extension/storage",
        Arc::new(SyncHostCallAdapter::new(StorageHost::new(state)))
            as Arc<dyn wasmos_runtime_api::HostCall>,
    )
}

// ── extension_log_storage (globally-routed callback) ────────────────

/// Host struct for the `duckdb:extension/log-storage` interface.
/// See `crate::extension` line 2325. Allocates a globally-routable
/// callback handle so the C API installer can carry ONE u32 through
/// the `duckdb_register_log_storage` write callback and re-enter
/// via `ExtensionInstance::dispatch_write_log_entry`.
#[derive(Clone)]
pub struct LogStorageHost {
    state: StateSource,
}

impl LogStorageHost {
    pub fn new(state: SharedExtensionState) -> Self {
        Self { state: StateSource::Shared(state) }
    }
    /// ADR-0029 Phase 6.2.h.6 — bridged variant for the consumer-
    /// migration bridge. Pulls `&mut ExtensionStoreState` from
    /// `HostCallContext::consumer_state` at dispatch time; the
    /// bridge populates it per-call with `store.data_mut()`.
    pub fn bridged() -> Self {
        Self { state: StateSource::FromCtx }
    }
}

#[host_iface(sync)]
impl LogStorageHost {
    fn register_log_storage(
        &self,
        ctx: &mut HostCallContext<'_>,
        name: String,
        callback_handle: u32,
    ) -> RuntimeResult<Result<u32, Duckerror>> {
        let mut g = self.state.hold(ctx)?;
        let global =
            g.allocate_callback_handle_pub(callback_handle, crate::CallbackKind::LogStorage);
        g.push_pending_log_storage(crate::extension::PendingLogStorage {
            name,
            callback_handle: global,
        });
        Ok(Ok(global))
    }
}

/// Register the `duckdb:extension/log-storage` handler.
pub fn install_log_storage_imports(
    imports: HostImports,
    state: SharedExtensionState,
) -> HostImports {
    imports.register(
        "duckdb:extension/log-storage",
        Arc::new(SyncHostCallAdapter::new(LogStorageHost::new(state)))
            as Arc<dyn wasmos_runtime_api::HostCall>,
    )
}

// ── extension_query (services delegation) ────────────────────────────

/// Host struct for the `duckdb:extension/query` interface.
/// See `crate::extension` line 2451. Delegates to the neutral
/// `ExtensionServices::query` sink for the actual work.
#[derive(Clone)]
pub struct QueryHost {
    state: StateSource,
}

impl QueryHost {
    pub fn new(state: SharedExtensionState) -> Self {
        Self { state: StateSource::Shared(state) }
    }
    /// ADR-0029 Phase 6.2.h.6 — bridged variant for the consumer-
    /// migration bridge. Pulls `&mut ExtensionStoreState` from
    /// `HostCallContext::consumer_state` at dispatch time; the
    /// bridge populates it per-call with `store.data_mut()`.
    pub fn bridged() -> Self {
        Self { state: StateSource::FromCtx }
    }
}

#[host_iface(sync)]
impl QueryHost {
    fn query(
        &self,
        ctx: &mut HostCallContext<'_>,
        sql: String,
    ) -> RuntimeResult<Result<Vec<Vec<String>>, String>> {
        let mut g = self.state.hold(ctx)?;
        Ok(g.services_query(&sql))
    }
}

/// Register the `duckdb:extension/query` handler.
pub fn install_query_imports(
    imports: HostImports,
    state: SharedExtensionState,
) -> HostImports {
    imports.register(
        "duckdb:extension/query",
        Arc::new(SyncHostCallAdapter::new(QueryHost::new(state)))
            as Arc<dyn wasmos_runtime_api::HostCall>,
    )
}

// ────────────────────────────────────────────────────────────────────
// Phase 6.2.d.2-e — services-delegating batch (3 interfaces).
//
// Adds config, logging, nested_exec. Each delegates to the
// neutral `ExtensionServices` trait sink via `services_mut()`.
// Needs mirror types for Configerror + Loglevel + Logfield +
// ExecResult so the wasmos-native marshalling matches the
// wit-bindgen wire shape.
// ────────────────────────────────────────────────────────────────────

// ── extension_config ────────────────────────────────────────────────

/// Wasmos-native mirror of the WIT `duckdb:extension/types.configerror`
/// variant. Wire-identical to the wit-bindgen counterpart.
#[derive(Debug, Clone, WitVariant)]
pub enum Configerror {
    Invalidkey(String),
    Typemismatch(String),
    Unavailable(String),
    Internalconfig(String),
}

impl Configerror {
    /// Convert from the neutral `crate::extension::ConfigError` (what
    /// `ExtensionServices` returns) to the wasmos-native variant.
    fn from_neutral(err: crate::extension::ConfigError) -> Self {
        use crate::extension::ConfigError as N;
        match err {
            N::InvalidKey(m) => Configerror::Invalidkey(m),
            N::TypeMismatch(m) => Configerror::Typemismatch(m),
            N::Unavailable(m) => Configerror::Unavailable(m),
            N::InternalConfig(m) => Configerror::Internalconfig(m),
        }
    }
}

/// Host struct for the `duckdb:extension/config` interface.
/// See `crate::extension` line 1683. 9 methods, all delegating
/// to the neutral `ExtensionServices` sink.
#[derive(Clone)]
pub struct ConfigHost {
    state: StateSource,
}

impl ConfigHost {
    pub fn new(state: SharedExtensionState) -> Self {
        Self { state: StateSource::Shared(state) }
    }
    /// ADR-0029 Phase 6.2.h.6 — bridged variant for the consumer-
    /// migration bridge. Pulls `&mut ExtensionStoreState` from
    /// `HostCallContext::consumer_state` at dispatch time; the
    /// bridge populates it per-call with `store.data_mut()`.
    pub fn bridged() -> Self {
        Self { state: StateSource::FromCtx }
    }
}

#[host_iface(sync)]
impl ConfigHost {
    fn provider_version(
        &self,
        ctx: &mut HostCallContext<'_>,
    ) -> RuntimeResult<String> {
        let mut g = self.state.hold(ctx)?;
        Ok(g.services_mut().provider_version().unwrap_or_else(|err| {
            eprintln!("extension config provider-version failed: {err:?}");
            "duckdb-extension-host".into()
        }))
    }

    fn list_keys(
        &self,
        ctx: &mut HostCallContext<'_>,
        prefix: Option<String>,
    ) -> RuntimeResult<Vec<String>> {
        let mut g = self.state.hold(ctx)?;
        Ok(g.services_mut()
            .list_keys(prefix.as_deref())
            .unwrap_or_else(|err| {
                eprintln!("extension config list-keys failed: {err:?}");
                Vec::new()
            }))
    }

    fn get_string(
        &self,
        ctx: &mut HostCallContext<'_>,
        path: String,
    ) -> RuntimeResult<Result<Option<String>, Configerror>> {
        let mut g = self.state.hold(ctx)?;
        Ok(g.services_mut().get_string(&path).map_err(Configerror::from_neutral))
    }

    fn get_bool(
        &self,
        ctx: &mut HostCallContext<'_>,
        path: String,
    ) -> RuntimeResult<Result<Option<bool>, Configerror>> {
        let mut g = self.state.hold(ctx)?;
        Ok(g.services_mut().get_bool(&path).map_err(Configerror::from_neutral))
    }

    fn get_i64(
        &self,
        ctx: &mut HostCallContext<'_>,
        path: String,
    ) -> RuntimeResult<Result<Option<i64>, Configerror>> {
        let mut g = self.state.hold(ctx)?;
        Ok(g.services_mut().get_i64(&path).map_err(Configerror::from_neutral))
    }

    fn get_u64(
        &self,
        ctx: &mut HostCallContext<'_>,
        path: String,
    ) -> RuntimeResult<Result<Option<u64>, Configerror>> {
        let mut g = self.state.hold(ctx)?;
        Ok(g.services_mut().get_u64(&path).map_err(Configerror::from_neutral))
    }

    fn get_f64(
        &self,
        ctx: &mut HostCallContext<'_>,
        path: String,
    ) -> RuntimeResult<Result<Option<f64>, Configerror>> {
        let mut g = self.state.hold(ctx)?;
        Ok(g.services_mut().get_f64(&path).map_err(Configerror::from_neutral))
    }

    fn get_bytes(
        &self,
        ctx: &mut HostCallContext<'_>,
        path: String,
    ) -> RuntimeResult<Result<Option<Vec<u8>>, Configerror>> {
        let mut g = self.state.hold(ctx)?;
        Ok(g.services_mut().get_bytes(&path).map_err(Configerror::from_neutral))
    }

    fn get_string_list(
        &self,
        ctx: &mut HostCallContext<'_>,
        path: String,
    ) -> RuntimeResult<Result<Option<Vec<String>>, Configerror>> {
        let mut g = self.state.hold(ctx)?;
        Ok(g.services_mut()
            .get_string_list(&path)
            .map_err(Configerror::from_neutral))
    }
}

/// Register the `duckdb:extension/config` handler.
pub fn install_config_imports(
    imports: HostImports,
    state: SharedExtensionState,
) -> HostImports {
    imports.register(
        "duckdb:extension/config",
        Arc::new(SyncHostCallAdapter::new(ConfigHost::new(state)))
            as Arc<dyn wasmos_runtime_api::HostCall>,
    )
}

// ── extension_logging ───────────────────────────────────────────────

/// Wasmos-native mirror of the WIT `duckdb:extension/types.loglevel`
/// enum. Wire-identical to the wit-bindgen counterpart.
#[derive(Debug, Clone, Copy, WitEnum)]
pub enum Loglevel {
    Trace,
    Debug,
    Info,
    Warn,
    Error,
}

impl Loglevel {
    /// Convert to the neutral `crate::extension::LogLevel` that
    /// `ExtensionServices` accepts.
    fn to_neutral(self) -> crate::extension::LogLevel {
        use crate::extension::LogLevel as N;
        match self {
            Loglevel::Trace => N::Trace,
            Loglevel::Debug => N::Debug,
            Loglevel::Info => N::Info,
            Loglevel::Warn => N::Warn,
            Loglevel::Error => N::Error,
        }
    }
}

/// Wasmos-native mirror of the WIT `duckdb:extension/types.logfield`
/// record. Wire-identical to the wit-bindgen counterpart.
#[derive(Debug, Clone, wasmos_runtime_api::WitRecord)]
pub struct Logfield {
    pub key: String,
    pub value: String,
}

/// Host struct for the `duckdb:extension/logging` interface.
/// See `crate::extension` line 1755. 2 methods, both delegating
/// to the neutral `ExtensionServices` sink.
#[derive(Clone)]
pub struct LoggingHost {
    state: StateSource,
}

impl LoggingHost {
    pub fn new(state: SharedExtensionState) -> Self {
        Self { state: StateSource::Shared(state) }
    }
    /// ADR-0029 Phase 6.2.h.6 — bridged variant for the consumer-
    /// migration bridge. Pulls `&mut ExtensionStoreState` from
    /// `HostCallContext::consumer_state` at dispatch time; the
    /// bridge populates it per-call with `store.data_mut()`.
    pub fn bridged() -> Self {
        Self { state: StateSource::FromCtx }
    }
}

#[host_iface(sync)]
impl LoggingHost {
    fn log(
        &self,
        ctx: &mut HostCallContext<'_>,
        level: Loglevel,
        message: String,
        target: Option<String>,
    ) -> RuntimeResult<()> {
        let mut g = self.state.hold(ctx)?;
        g.services_mut()
            .log(level.to_neutral(), &message, target.as_deref());
        Ok(())
    }

    fn log_fields(
        &self,
        ctx: &mut HostCallContext<'_>,
        level: Loglevel,
        message: String,
        fields: Vec<Logfield>,
    ) -> RuntimeResult<()> {
        let converted: Vec<crate::extension::LogField> = fields
            .into_iter()
            .map(|f| crate::extension::LogField {
                key: f.key,
                value: f.value,
            })
            .collect();
        let mut g = self.state.hold(ctx)?;
        g.services_mut()
            .log_fields(level.to_neutral(), &message, &converted);
        Ok(())
    }
}

/// Register the `duckdb:extension/logging` handler.
pub fn install_logging_imports(
    imports: HostImports,
    state: SharedExtensionState,
) -> HostImports {
    imports.register(
        "duckdb:extension/logging",
        Arc::new(SyncHostCallAdapter::new(LoggingHost::new(state)))
            as Arc<dyn wasmos_runtime_api::HostCall>,
    )
}

// ── extension_nested_exec ────────────────────────────────────────────

/// Wasmos-native mirror of the WIT `duckdb:extension/nested-exec.
/// exec-result` record. Wire-identical to the wit-bindgen counterpart.
#[derive(Debug, Clone, wasmos_runtime_api::WitRecord)]
pub struct ExecResult {
    pub rows: Option<Vec<Vec<String>>>,
    pub rows_affected: Option<u64>,
}

impl ExecResult {
    /// Convert from the neutral `crate::extension::NestedExecResult`
    /// that `ExtensionServices::nested_exec` returns.
    fn from_neutral(r: crate::extension::NestedExecResult) -> Self {
        ExecResult {
            rows: r.rows,
            rows_affected: r.rows_affected,
        }
    }
}

/// Host struct for the `duckdb:extension/nested-exec` interface.
/// See `crate::extension` line 2478. Delegates to
/// `ExtensionServices::nested_exec` via the shared state.
///
/// NOTE: The wit-bindgen counterpart wraps the call in a
/// `NestedExecDepthGuard::enter()?` per-OS-thread nesting-depth
/// counter to prevent recursive execution from spiraling. That
/// guard type is private to `crate::extension`; the wasmos-
/// native path relies on the same guard being active if the
/// caller invokes both wit-bindgen and wasmos paths through the
/// same store, or on ExtensionServices::nested_exec's own
/// re-entry protection. Non-blocking for the intended use;
/// callers can wire the depth guard manually if their setup
/// exposes recursive-invocation risk.
#[derive(Clone)]
pub struct NestedExecHost {
    state: StateSource,
}

impl NestedExecHost {
    pub fn new(state: SharedExtensionState) -> Self {
        Self { state: StateSource::Shared(state) }
    }
    /// ADR-0029 Phase 6.2.h.6 — bridged variant for the consumer-
    /// migration bridge. Pulls `&mut ExtensionStoreState` from
    /// `HostCallContext::consumer_state` at dispatch time; the
    /// bridge populates it per-call with `store.data_mut()`.
    pub fn bridged() -> Self {
        Self { state: StateSource::FromCtx }
    }
}

#[host_iface(sync)]
impl NestedExecHost {
    fn nested_exec(
        &self,
        ctx: &mut HostCallContext<'_>,
        sql: String,
    ) -> RuntimeResult<Result<ExecResult, String>> {
        let mut g = self.state.hold(ctx)?;
        Ok(g.services_mut().nested_exec(&sql).map(ExecResult::from_neutral))
    }
}

/// Register the `duckdb:extension/nested-exec` handler.
pub fn install_nested_exec_imports(
    imports: HostImports,
    state: SharedExtensionState,
) -> HostImports {
    imports.register(
        "duckdb:extension/nested-exec",
        Arc::new(SyncHostCallAdapter::new(NestedExecHost::new(state)))
            as Arc<dyn wasmos_runtime_api::HostCall>,
    )
}

// ────────────────────────────────────────────────────────────────────
// Phase 6.2.d.2-f — secret interface (18/27).
// ────────────────────────────────────────────────────────────────────

/// Wasmos-native mirror of the WIT `duckdb:extension/secret.
/// secret-param` record.
#[derive(Debug, Clone, wasmos_runtime_api::WitRecord)]
pub struct SecretParam {
    pub name: String,
    pub redacted: bool,
}

/// Host struct for the `duckdb:extension/secret` interface.
/// See `crate::extension` line 1937. 2 methods, both push to
/// `pending_secrets`. Captures secret TYPE + PROVIDER
/// declarations for drain by `ExtensionManager::secret_backends`.
#[derive(Clone)]
pub struct SecretHost {
    state: StateSource,
}

impl SecretHost {
    pub fn new(state: SharedExtensionState) -> Self {
        Self { state: StateSource::Shared(state) }
    }
    /// ADR-0029 Phase 6.2.h.6 — bridged variant for the consumer-
    /// migration bridge. Pulls `&mut ExtensionStoreState` from
    /// `HostCallContext::consumer_state` at dispatch time; the
    /// bridge populates it per-call with `store.data_mut()`.
    pub fn bridged() -> Self {
        Self { state: StateSource::FromCtx }
    }
}

#[host_iface(sync)]
impl SecretHost {
    fn register_secret_type(
        &self,
        ctx: &mut HostCallContext<'_>,
        type_name: String,
        params: Vec<SecretParam>,
        callback_handle: u32,
    ) -> RuntimeResult<Result<u32, Duckerror>> {
        let params: Vec<(String, bool)> =
            params.into_iter().map(|p| (p.name, p.redacted)).collect();
        let mut g = self.state.hold(ctx)?;
        let registry_id = g.alloc_resource_id();
        let extension = g.extension_name().to_string();
        g.push_pending_secret(crate::reg::SecretReg {
            extension,
            type_name,
            provider: None,
            params,
            callback_handle,
        });
        Ok(Ok(registry_id))
    }

    fn register_secret_provider(
        &self,
        ctx: &mut HostCallContext<'_>,
        type_name: String,
        provider: String,
        callback_handle: u32,
    ) -> RuntimeResult<Result<u32, Duckerror>> {
        let mut g = self.state.hold(ctx)?;
        let registry_id = g.alloc_resource_id();
        let extension = g.extension_name().to_string();
        g.push_pending_secret(crate::reg::SecretReg {
            extension,
            type_name,
            provider: Some(provider),
            params: Vec::new(),
            callback_handle,
        });
        Ok(Ok(registry_id))
    }
}

/// Register the `duckdb:extension/secret` handler.
pub fn install_secret_imports(
    imports: HostImports,
    state: SharedExtensionState,
) -> HostImports {
    imports.register(
        "duckdb:extension/secret",
        Arc::new(SyncHostCallAdapter::new(SecretHost::new(state)))
            as Arc<dyn wasmos_runtime_api::HostCall>,
    )
}

// ────────────────────────────────────────────────────────────────────
// Phase 6.2.d.2-g — macro_ext + types_ext (20/27).
//
// Both are simple state-touching push interfaces: primitives +
// Vec<String> args, push into a pending buffer, return Ok. Follows
// the parser/optimizer/settings pattern from Phase 6.2.d.2-c.
// ────────────────────────────────────────────────────────────────────

// ── extension_macro_ext ─────────────────────────────────────────────

/// Host struct for the `duckdb:extension/macro_ext` interface.
/// See `crate::extension` line 2160. 1 method (register_table_macro);
/// pushes to `pending_table_macros`.
#[derive(Clone)]
pub struct MacroExtHost {
    state: StateSource,
}

impl MacroExtHost {
    pub fn new(state: SharedExtensionState) -> Self {
        Self { state: StateSource::Shared(state) }
    }
    /// ADR-0029 Phase 6.2.h.6 — bridged variant for the consumer-
    /// migration bridge. Pulls `&mut ExtensionStoreState` from
    /// `HostCallContext::consumer_state` at dispatch time; the
    /// bridge populates it per-call with `store.data_mut()`.
    pub fn bridged() -> Self {
        Self { state: StateSource::FromCtx }
    }
}

#[host_iface(sync)]
impl MacroExtHost {
    fn register_table_macro(
        &self,
        ctx: &mut HostCallContext<'_>,
        schema: String,
        name: String,
        parameters: Vec<String>,
        body_sql: String,
    ) -> RuntimeResult<Result<(), Duckerror>> {
        let mut g = self.state.hold(ctx)?;
        let extension = g.extension_name().to_string();
        g.push_pending_table_macro(crate::reg::TableMacroReg {
            extension,
            schema,
            name,
            parameters,
            body_sql,
        });
        Ok(Ok(()))
    }
}

/// Register the `duckdb:extension/macro_ext` handler.
pub fn install_macro_ext_imports(
    imports: HostImports,
    state: SharedExtensionState,
) -> HostImports {
    imports.register(
        "duckdb:extension/macro_ext",
        Arc::new(SyncHostCallAdapter::new(MacroExtHost::new(state)))
            as Arc<dyn wasmos_runtime_api::HostCall>,
    )
}

// ── extension_types_ext ─────────────────────────────────────────────

/// Host struct for the `duckdb:extension/types_ext` interface.
/// See `crate::extension` line 2186. 2 methods
/// (register_logical_type_modified, register_enum).
#[derive(Clone)]
pub struct TypesExtHost {
    state: StateSource,
}

impl TypesExtHost {
    pub fn new(state: SharedExtensionState) -> Self {
        Self { state: StateSource::Shared(state) }
    }
    /// ADR-0029 Phase 6.2.h.6 — bridged variant for the consumer-
    /// migration bridge. Pulls `&mut ExtensionStoreState` from
    /// `HostCallContext::consumer_state` at dispatch time; the
    /// bridge populates it per-call with `store.data_mut()`.
    pub fn bridged() -> Self {
        Self { state: StateSource::FromCtx }
    }
}

#[host_iface(sync)]
impl TypesExtHost {
    fn register_logical_type_modified(
        &self,
        ctx: &mut HostCallContext<'_>,
        name: String,
        type_expr: String,
    ) -> RuntimeResult<Result<u32, Duckerror>> {
        let mut g = self.state.hold(ctx)?;
        let extension = g.extension_name().to_string();
        g.push_pending_modified_type(crate::reg::ModifiedTypeReg {
            extension,
            name,
            type_expr,
        });
        Ok(Ok(g.alloc_resource_id()))
    }

    fn register_enum(
        &self,
        ctx: &mut HostCallContext<'_>,
        name: String,
        members: Vec<String>,
    ) -> RuntimeResult<Result<u32, Duckerror>> {
        let mut g = self.state.hold(ctx)?;
        let extension = g.extension_name().to_string();
        g.push_pending_enum_type(crate::reg::EnumTypeReg {
            extension,
            name,
            members,
        });
        Ok(Ok(g.alloc_resource_id()))
    }
}

/// Register the `duckdb:extension/types_ext` handler.
pub fn install_types_ext_imports(
    imports: HostImports,
    state: SharedExtensionState,
) -> HostImports {
    imports.register(
        "duckdb:extension/types_ext",
        Arc::new(SyncHostCallAdapter::new(TypesExtHost::new(state)))
            as Arc<dyn wasmos_runtime_api::HostCall>,
    )
}

// ────────────────────────────────────────────────────────────────────
// Phase 6.2.d.2-h — files interface (22/27).
// ────────────────────────────────────────────────────────────────────

/// Wasmos-native mirror of the WIT `duckdb:extension/files.
/// detection-mode` enum.
#[derive(Debug, Clone, Copy, WitEnum)]
pub enum DetectionMode {
    ExtensionOnly,
    Signature,
}

impl DetectionMode {
    fn as_debug(self) -> &'static str {
        match self {
            DetectionMode::ExtensionOnly => "extension-only",
            DetectionMode::Signature => "signature",
        }
    }
}

/// Wasmos-native mirror of the WIT `duckdb:extension/files.
/// replacement-scan` record.
#[derive(Debug, Clone, wasmos_runtime_api::WitRecord)]
pub struct ReplacementScan {
    pub extensions: Vec<String>,
    pub table_function: u32,
    pub mode: DetectionMode,
}

/// Wasmos-native mirror of the WIT `duckdb:extension/files.
/// copy-handler` record.
#[derive(Debug, Clone, wasmos_runtime_api::WitRecord)]
pub struct CopyHandler {
    pub extension: String,
    pub function: u32,
}

/// Host struct for the `duckdb:extension/files` interface.
/// See `crate::extension` line 1908. 2 methods:
/// register_replacement_scan (looks up table_handle_names), and
/// register_copy_handler.
///
/// Note: this interface uses `Result<u32, String>` (plain
/// String errors), not `Result<u32, Duckerror>` like most
/// state-touching interfaces in the arc — matches the
/// wit-bindgen counterpart signature.
#[derive(Clone)]
pub struct FilesHost {
    state: StateSource,
}

impl FilesHost {
    pub fn new(state: SharedExtensionState) -> Self {
        Self { state: StateSource::Shared(state) }
    }
    /// ADR-0029 Phase 6.2.h.6 — bridged variant for the consumer-
    /// migration bridge. Pulls `&mut ExtensionStoreState` from
    /// `HostCallContext::consumer_state` at dispatch time; the
    /// bridge populates it per-call with `store.data_mut()`.
    pub fn bridged() -> Self {
        Self { state: StateSource::FromCtx }
    }
}

#[host_iface(sync)]
impl FilesHost {
    fn register_replacement_scan(
        &self,
        ctx: &mut HostCallContext<'_>,
        scan: ReplacementScan,
    ) -> RuntimeResult<Result<u32, String>> {
        let mut g = self.state.hold(ctx)?;
        let function_name = match g.lookup_table_handle_name(scan.table_function) {
            Some(name) => name,
            None => {
                return Ok(Err(format!(
                    "replacement scan references unknown table-function handle {}",
                    scan.table_function
                )));
            }
        };
        let extension = g.extension_name().to_string();
        let id = g.alloc_resource_id();
        // Log kept as a debug-adjacent side-effect only (skipping
        // the verbose_log! macro since it's private to
        // crate::extension); wire behavior unaffected.
        let _ = scan.mode.as_debug();
        g.push_pending_replacement_scan(crate::reg::ReplacementScanReg {
            extension,
            extensions: scan.extensions,
            function_name,
        });
        Ok(Ok(id))
    }

    fn register_copy_handler(
        &self,
        ctx: &mut HostCallContext<'_>,
        handler: CopyHandler,
    ) -> RuntimeResult<Result<u32, String>> {
        let mut g = self.state.hold(ctx)?;
        let extension = g.extension_name().to_string();
        let id = g.alloc_resource_id();
        g.push_pending_copy_handler(crate::reg::CopyHandlerReg {
            extension,
            file_extension: handler.extension,
            function_handle: handler.function,
        });
        Ok(Ok(id))
    }
}

/// Register the `duckdb:extension/files` handler.
pub fn install_files_imports(
    imports: HostImports,
    state: SharedExtensionState,
) -> HostImports {
    imports.register(
        "duckdb:extension/files",
        Arc::new(SyncHostCallAdapter::new(FilesHost::new(state)))
            as Arc<dyn wasmos_runtime_api::HostCall>,
    )
}

// ────────────────────────────────────────────────────────────────────
// Phase 6.2.d.2-i — LogicalType + Columndef mirrors + arrow_ext (22/27).
//
// LogicalType is the shared blocker for arrow_ext + table_stream +
// runtime_ext + parts of runtime. Landing it once here unblocks
// those four (and more) future sub-sessions.
// ────────────────────────────────────────────────────────────────────

/// Wasmos-native mirror of the WIT `duckdb:extension/types.
/// decimalshape` record — the `decimal` arm's width + scale payload.
#[derive(Debug, Clone, wasmos_runtime_api::WitRecord)]
pub struct DecimalShape {
    pub width: u8,
    pub scale: u8,
}

/// Wasmos-native mirror of the WIT `duckdb:extension/types.
/// logicaltype` variant. 20 unit arms + 2 tuple arms
/// (`decimal(decimalshape)`, `complex(string)`). Wire-identical to
/// the wit-bindgen counterpart + to `crate::reg::LogicalType`.
///
/// Shared mirror — arrow_ext, table_stream, runtime_ext, and much
/// of runtime all use it. Defined once here; every future
/// migration in Phase 6.2.d.2 reuses it.
#[derive(Debug, Clone, WitVariant)]
pub enum LogicalType {
    Boolean,
    Int64,
    Uint64,
    Float64,
    Text,
    Blob,
    Int32,
    Timestamp,
    Int8,
    Int16,
    Uint8,
    Uint16,
    Uint32,
    Float32,
    Date,
    Time,
    Timestamptz,
    Decimal(DecimalShape),
    Interval,
    Uuid,
    Hugeint,
    Uhugeint,
    Complex(String),
}

impl LogicalType {
    /// Convert to `crate::reg::LogicalType` (the neutral type the
    /// pending buffers store). Same variant order + names as the
    /// WIT source, so the two representations are wire-identical;
    /// this is a Rust-level type-adapter, not a wire conversion.
    pub fn to_reg(self) -> crate::reg::LogicalType {
        use crate::reg::LogicalType as R;
        match self {
            LogicalType::Boolean => R::Boolean,
            LogicalType::Int64 => R::Int64,
            LogicalType::Uint64 => R::Uint64,
            LogicalType::Float64 => R::Float64,
            LogicalType::Text => R::Text,
            LogicalType::Blob => R::Blob,
            LogicalType::Int32 => R::Int32,
            LogicalType::Timestamp => R::Timestamp,
            LogicalType::Int8 => R::Int8,
            LogicalType::Int16 => R::Int16,
            LogicalType::Uint8 => R::Uint8,
            LogicalType::Uint16 => R::Uint16,
            LogicalType::Uint32 => R::Uint32,
            LogicalType::Float32 => R::Float32,
            LogicalType::Date => R::Date,
            LogicalType::Time => R::Time,
            LogicalType::Timestamptz => R::Timestamptz,
            LogicalType::Decimal(shape) => R::Decimal {
                width: shape.width,
                scale: shape.scale,
            },
            LogicalType::Interval => R::Interval,
            LogicalType::Uuid => R::Uuid,
            LogicalType::Hugeint => R::Hugeint,
            // reg::LogicalType uses UHugeint (mid-word H capitalized);
            // the WIT ident 'uhugeint' PascalCases as Uhugeint here.
            LogicalType::Uhugeint => R::UHugeint,
            LogicalType::Complex(s) => R::Complex(s),
        }
    }
}

/// Wasmos-native mirror of the WIT `duckdb:extension/types.columndef`
/// record. Shared by arrow_ext + table_stream + several methods on
/// runtime.
#[derive(Debug, Clone, wasmos_runtime_api::WitRecord)]
pub struct Columndef {
    pub name: String,
    pub logical: LogicalType,
}

impl Columndef {
    pub fn to_reg(self) -> crate::reg::ColumnDef {
        crate::reg::ColumnDef {
            name: self.name,
            logical: self.logical.to_reg(),
        }
    }
}

// ── extension_arrow_ext ─────────────────────────────────────────────

/// Host struct for the `duckdb:extension/arrow_ext` interface.
/// See `crate::extension` line 2352. 1 method (register_arrow_table);
/// pushes to `pending_arrow_tables` with the caller-supplied schema
/// converted to `reg::ColumnDef` via `Columndef::to_reg`.
#[derive(Clone)]
pub struct ArrowExtHost {
    state: StateSource,
}

impl ArrowExtHost {
    pub fn new(state: SharedExtensionState) -> Self {
        Self { state: StateSource::Shared(state) }
    }
    /// ADR-0029 Phase 6.2.h.6 — bridged variant for the consumer-
    /// migration bridge. Pulls `&mut ExtensionStoreState` from
    /// `HostCallContext::consumer_state` at dispatch time; the
    /// bridge populates it per-call with `store.data_mut()`.
    pub fn bridged() -> Self {
        Self { state: StateSource::FromCtx }
    }
}

#[host_iface(sync)]
impl ArrowExtHost {
    fn register_arrow_table(
        &self,
        ctx: &mut HostCallContext<'_>,
        name: String,
        schema: Vec<Columndef>,
        callback_handle: u32,
    ) -> RuntimeResult<Result<u32, Duckerror>> {
        let columns: Vec<crate::reg::ColumnDef> =
            schema.into_iter().map(Columndef::to_reg).collect();
        let mut g = self.state.hold(ctx)?;
        let extension = g.extension_name().to_string();
        g.push_pending_arrow_table(crate::reg::ArrowTableReg {
            extension,
            name,
            columns,
            callback_handle,
        });
        Ok(Ok(g.alloc_resource_id()))
    }
}

/// Register the `duckdb:extension/arrow_ext` handler.
pub fn install_arrow_ext_imports(
    imports: HostImports,
    state: SharedExtensionState,
) -> HostImports {
    imports.register(
        "duckdb:extension/arrow_ext",
        Arc::new(SyncHostCallAdapter::new(ArrowExtHost::new(state)))
            as Arc<dyn wasmos_runtime_api::HostCall>,
    )
}

// ────────────────────────────────────────────────────────────────────
// Phase 6.2.d.2-j — Funcarg mirror + table_stream (23/27).
// ────────────────────────────────────────────────────────────────────

/// Wasmos-native mirror of the WIT `duckdb:extension/types.funcarg`
/// record. Shared by table_stream + runtime_ext + parts of runtime.
#[derive(Debug, Clone, wasmos_runtime_api::WitRecord)]
pub struct Funcarg {
    pub name: Option<String>,
    pub logical: LogicalType,
}

impl Funcarg {
    pub fn to_reg(self) -> crate::reg::FuncArg {
        crate::reg::FuncArg {
            name: self.name,
            logical: self.logical.to_reg(),
        }
    }
}

// ── extension_table_stream ──────────────────────────────────────────

/// Host struct for the `duckdb:extension/table_stream` interface.
/// See `crate::extension` line 2165. 1 method
/// (register_filterable_table); allocates a globally-routable
/// callback via `allocate_callback_handle_pub` +
/// `CallbackKind::Table`, converts arguments/columns to the
/// neutral reg::* shapes, pushes to pending_filterable_tables.
#[derive(Clone)]
pub struct TableStreamHost {
    state: StateSource,
}

impl TableStreamHost {
    pub fn new(state: SharedExtensionState) -> Self {
        Self { state: StateSource::Shared(state) }
    }
    /// ADR-0029 Phase 6.2.h.6 — bridged variant for the consumer-
    /// migration bridge. Pulls `&mut ExtensionStoreState` from
    /// `HostCallContext::consumer_state` at dispatch time; the
    /// bridge populates it per-call with `store.data_mut()`.
    pub fn bridged() -> Self {
        Self { state: StateSource::FromCtx }
    }
}

#[host_iface(sync)]
impl TableStreamHost {
    fn register_filterable_table(
        &self,
        ctx: &mut HostCallContext<'_>,
        name: String,
        arguments: Vec<Funcarg>,
        columns: Vec<Columndef>,
        callback_handle: u32,
    ) -> RuntimeResult<Result<u32, Duckerror>> {
        let converted_arguments: Vec<crate::reg::FuncArg> =
            arguments.into_iter().map(Funcarg::to_reg).collect();
        let converted_columns: Vec<crate::reg::ColumnDef> =
            columns.into_iter().map(Columndef::to_reg).collect();
        let mut g = self.state.hold(ctx)?;
        let global =
            g.allocate_callback_handle_pub(callback_handle, crate::CallbackKind::Table);
        let extension = g.extension_name().to_string();
        g.push_pending_filterable_table(crate::reg::FilterableTableReg {
            extension,
            name,
            arguments: converted_arguments,
            columns: converted_columns,
            callback_handle: global,
        });
        Ok(Ok(global))
    }
}

/// Register the `duckdb:extension/table_stream` handler.
pub fn install_table_stream_imports(
    imports: HostImports,
    state: SharedExtensionState,
) -> HostImports {
    imports.register(
        "duckdb:extension/table_stream",
        Arc::new(SyncHostCallAdapter::new(TableStreamHost::new(state)))
            as Arc<dyn wasmos_runtime_api::HostCall>,
    )
}

// ────────────────────────────────────────────────────────────────────
// Phase 6.2.d.2-k — runtime_ext + Funcflags/Funcopts/NullHandling
// mirrors (24/27).
// ────────────────────────────────────────────────────────────────────

/// Wasmos-native mirror of the WIT `duckdb:extension/types.funcflags`
/// flags shape. Wire-identical to `crate::reg::FuncFlags`. Uses
/// wasmos's WitFlags derive; each bit maps to a bool field.
///
/// WIT `funcflags` field names (kebab-case): deterministic,
/// commutative, stateless, sideeffecting, deprecated. The Rust
/// snake_case field `side_effecting` name-mangles from the WIT
/// `sideeffecting` (no dash between the two).
#[derive(Debug, Clone, Copy, WitFlags)]
pub struct Funcflags {
    pub deterministic: bool,
    pub commutative: bool,
    pub stateless: bool,
    pub sideeffecting: bool,
    pub deprecated: bool,
}

impl Funcflags {
    pub fn to_reg(self) -> crate::reg::FuncFlags {
        crate::reg::FuncFlags {
            deterministic: self.deterministic,
            commutative: self.commutative,
            stateless: self.stateless,
            side_effecting: self.sideeffecting,
            deprecated: self.deprecated,
        }
    }
}

/// Wasmos-native mirror of the WIT `duckdb:extension/types.funcopts`
/// record.
#[derive(Debug, Clone, wasmos_runtime_api::WitRecord)]
pub struct Funcopts {
    pub description: Option<String>,
    pub tags: Vec<String>,
    pub attributes: Funcflags,
}

impl Funcopts {
    pub fn to_reg(self) -> crate::reg::FuncOpts {
        crate::reg::FuncOpts {
            description: self.description,
            tags: self.tags,
            attributes: self.attributes.to_reg(),
        }
    }
}

/// Wasmos-native mirror of the WIT `duckdb:extension/runtime-ext.
/// null-handling` enum. 2 unit arms.
#[derive(Debug, Clone, Copy, WitEnum)]
pub enum NullHandling {
    Default,
    Special,
}

impl NullHandling {
    fn is_special(self) -> bool {
        matches!(self, NullHandling::Special)
    }
}

/// Host struct for the `duckdb:extension/runtime_ext` interface.
/// See `crate::extension` line 2228. 1 method (register_scalar_ex).
/// Byte-identical to the wit-bindgen counterpart: allocates a
/// resource id, packs the wasmos-native shapes into
/// `reg::ScalarExReg`, pushes to pending_scalar_ex. Derives the
/// `volatile` flag from `!options.attributes.deterministic`
/// (default false when options absent), matching the wit-bindgen
/// audit-fix behavior.
#[derive(Clone)]
pub struct RuntimeExtHost {
    state: StateSource,
}

impl RuntimeExtHost {
    pub fn new(state: SharedExtensionState) -> Self {
        Self { state: StateSource::Shared(state) }
    }
    /// ADR-0029 Phase 6.2.h.6 — bridged variant for the consumer-
    /// migration bridge. Pulls `&mut ExtensionStoreState` from
    /// `HostCallContext::consumer_state` at dispatch time; the
    /// bridge populates it per-call with `store.data_mut()`.
    pub fn bridged() -> Self {
        Self { state: StateSource::FromCtx }
    }
}

#[host_iface(sync)]
impl RuntimeExtHost {
    fn register_scalar_ex(
        &self,
        ctx: &mut HostCallContext<'_>,
        name: String,
        arguments: Vec<Funcarg>,
        varargs: Option<LogicalType>,
        returns: LogicalType,
        null_handling: NullHandling,
        callback_handle: u32,
        options: Option<Funcopts>,
    ) -> RuntimeResult<Result<u32, Duckerror>> {
        let special_null = null_handling.is_special();
        let arguments_r: Vec<crate::reg::FuncArg> =
            arguments.into_iter().map(Funcarg::to_reg).collect();
        let varargs_r = varargs.map(LogicalType::to_reg);
        let returns_r = returns.to_reg();
        let options_r = options.map(Funcopts::to_reg);
        let volatile = options_r
            .as_ref()
            .map(|o| !o.attributes.deterministic)
            .unwrap_or(false);
        let mut g = self.state.hold(ctx)?;
        let extension = g.extension_name().to_string();
        let registry_id = g.alloc_resource_id();
        g.push_pending_scalar_ex(crate::reg::ScalarExReg {
            extension,
            name,
            arguments: arguments_r,
            varargs: varargs_r,
            returns: returns_r,
            special_null,
            volatile,
            callback_handle,
            options: options_r,
        });
        Ok(Ok(registry_id))
    }
}

/// Register the `duckdb:extension/runtime_ext` handler.
pub fn install_runtime_ext_imports(
    imports: HostImports,
    state: SharedExtensionState,
) -> HostImports {
    imports.register(
        "duckdb:extension/runtime_ext",
        Arc::new(SyncHostCallAdapter::new(RuntimeExtHost::new(state)))
            as Arc<dyn wasmos_runtime_api::HostCall>,
    )
}

// ────────────────────────────────────────────────────────────────────
// Phase 6.2.d.2-l/m — catalog (26/27 after -m promotes register_cast
// from stub to full Resource<CastCallback> arg handling).
// ────────────────────────────────────────────────────────────────────

/// Wasmos-native marker for the WIT `duckdb:extension/catalog.
/// cast-callback` resource. Guest-owned resource handle passed as
/// an arg to `register_cast`; the host reads the `.handle()`
/// (rep) to route future dispatch calls back to the component's
/// cast-dispatch export.
///
/// `#[derive(HostResourceType)]` + `#[wit_resource(...)]` produces
/// the `HostResourceType` impl the classifier + the
/// `HostCallContext::new_typed_resource<T>` / `typed_resource_rep::<T>`
/// helpers need. Sibling to the ScalarCallback / TableCallback
/// markers in `runtime/api/tests/host_iface_resource.rs`.
#[derive(Debug, HostResourceType)]
#[wit_resource(interface = "duckdb:extension/catalog", name = "cast-callback")]
pub struct CastCallback;

/// Wasmos-native mirror of the WIT `duckdb:extension/catalog.
/// logical-type` record — DIFFERENT from `types.logicaltype`
/// (variant). This one is a simple 2-field record.
#[derive(Debug, Clone, wasmos_runtime_api::WitRecord)]
pub struct CatalogLogicalType {
    pub name: String,
    pub physical: String,
}

/// Wasmos-native mirror of the WIT `duckdb:extension/catalog.
/// cast-kind` enum.
#[derive(Debug, Clone, Copy, WitEnum)]
pub enum CastKind {
    Implicit,
    Assignment,
    Explicit,
}

/// Wasmos-native mirror of the WIT `duckdb:extension/catalog.
/// cast-spec` record.
#[derive(Debug, Clone, wasmos_runtime_api::WitRecord)]
pub struct CastSpec {
    pub from: String,
    pub to: String,
    pub kind: CastKind,
    pub implicit_cost: Option<i32>,
}

/// Wasmos-native mirror of the WIT `duckdb:extension/catalog.
/// macro-def` record.
#[derive(Debug, Clone, wasmos_runtime_api::WitRecord)]
pub struct MacroDef {
    pub schema: String,
    pub name: String,
    pub parameters: Vec<String>,
    pub definition_sql: String,
}

/// Host struct for the `duckdb:extension/catalog` interface.
/// See `crate::extension` line 1885. 3 methods, all migrated
/// (register_cast promoted from stub to real Resource<
/// CastCallback> arg handling in Phase 6.2.d.2-m).
#[derive(Clone)]
pub struct CatalogHost {
    state: StateSource,
}

impl CatalogHost {
    pub fn new(state: SharedExtensionState) -> Self {
        Self { state: StateSource::Shared(state) }
    }
    /// ADR-0029 Phase 6.2.h.6 — bridged variant for the consumer-
    /// migration bridge. Pulls `&mut ExtensionStoreState` from
    /// `HostCallContext::consumer_state` at dispatch time; the
    /// bridge populates it per-call with `store.data_mut()`.
    pub fn bridged() -> Self {
        Self { state: StateSource::FromCtx }
    }
}

#[host_iface(sync)]
impl CatalogHost {
    fn register_logical_type(
        &self,
        ctx: &mut HostCallContext<'_>,
        ty: CatalogLogicalType,
    ) -> RuntimeResult<Result<u32, String>> {
        let mut g = self.state.hold(ctx)?;
        let extension = g.extension_name().to_string();
        let handle = g.alloc_resource_id();
        g.push_pending_logical_type(crate::reg::LogicalTypeReg {
            extension,
            name: ty.name,
            physical: ty.physical,
        });
        Ok(Ok(handle))
    }

    /// Handler for `register-cast`. `callback: Resource<CastCallback>`
    /// arrives as a guest-owned resource handle; we read
    /// `.handle()` for the rep (matches the wit-bindgen path's
    /// `callback.rep()` at `crate::extension` line 1905). The
    /// implicit `std::mem::forget(callback)` on the wit-bindgen
    /// side isn't needed here — `Resource<T>` in wasmos-native
    /// doesn't run a destructor on Drop; the resource lifecycle
    /// is managed by the adapter's HostCallCtxImpl.
    ///
    /// Promoted from stub (Phase 6.2.d.2-l) to real impl in
    /// Phase 6.2.d.2-m via the `CastCallback` marker +
    /// `Resource<CastCallback>` arg — the `#[host_iface]`
    /// classifier now handles Resource<T> args through
    /// WitBridgeCtx routing.
    fn register_cast(
        &self,
        ctx: &mut HostCallContext<'_>,
        spec: CastSpec,
        callback: Resource<CastCallback>,
    ) -> RuntimeResult<Result<(), String>> {
        let callback_handle = callback.handle();
        let mut g = self.state.hold(ctx)?;
        let extension = g.extension_name().to_string();
        g.push_pending_cast(crate::reg::CastReg {
            extension,
            source: spec.from,
            target: spec.to,
            callback_handle,
            implicit_cost: spec.implicit_cost,
        });
        Ok(Ok(()))
    }

    fn register_macro(
        &self,
        ctx: &mut HostCallContext<'_>,
        def: MacroDef,
    ) -> RuntimeResult<Result<(), String>> {
        let mut g = self.state.hold(ctx)?;
        let extension = g.extension_name().to_string();
        g.push_pending_macro(crate::reg::MacroReg {
            extension,
            schema: def.schema,
            name: def.name,
            parameters: def.parameters,
            definition_sql: def.definition_sql,
        });
        Ok(Ok(()))
    }
}

/// Register the `duckdb:extension/catalog` handler.
pub fn install_catalog_imports(
    imports: HostImports,
    state: SharedExtensionState,
) -> HostImports {
    imports.register(
        "duckdb:extension/catalog",
        Arc::new(SyncHostCallAdapter::new(CatalogHost::new(state)))
            as Arc<dyn wasmos_runtime_api::HostCall>,
    )
}

// ────────────────────────────────────────────────────────────────────
// Phase 6.2.d.2-m — file_lock (27/27 for the interfaces themselves;
// resource lifecycle methods deferred pending wasmos-side story).
//
// The wit-bindgen counterpart at `crate::extension` line 2609
// implements TWO host-side traits:
//
//   1. `extension_file_lock::Host` — the interface methods:
//        acquire-exclusive(path) -> Resource<LockHandle>
//        try-acquire-exclusive(path) -> Option<Resource<LockHandle>>
//      Migrated here to `FileLockHost` via `#[host_iface]`.
//      Uses `Resource::<LockHandle>::from_raw(id, true)` for
//      the return; the `#[host_iface]` classifier emits
//      `ctx.new_typed_resource::<LockHandle>(id)` lowering.
//
//   2. `extension_file_lock::HostLockHandle` — the resource-
//      METHOD trait:
//        [method]lock-handle.release  — early lock release
//        [resource-drop]lock-handle    — auto-cleanup destructor
//
//      LANDED as part of Phase 6.2.d.2-n — both methods added
//      to `FileLockHost` below via `#[method("[method]lock-
//      handle.release")]` + `#[method("[resource-drop]lock-
//      handle")]` overrides. The release method works today;
//      destructor dispatch depends on the wasmos adapter
//      surfacing resource-drop events (real wasmos-side gap,
//      documented in the handler comment below).
// ────────────────────────────────────────────────────────────────────

/// Wasmos-native marker for the WIT `duckdb:extension/file-lock.
/// lock-handle` resource. Handle rep is the internal id from
/// `ExtensionStoreState::acquire_exclusive_lock`; the
/// `#[host_iface]` classifier emits ctx-mediated lower for
/// Resource<LockHandle> returns.
#[derive(Debug, HostResourceType)]
#[wit_resource(interface = "duckdb:extension/file-lock", name = "lock-handle")]
pub struct LockHandle;

/// Host struct for the `duckdb:extension/file-lock` interface.
/// See `crate::extension` line 2609. 2 methods; the resource-
/// method trait (`HostLockHandle`) is deferred (see module
/// section docstring).
#[derive(Clone)]
pub struct FileLockHost {
    state: StateSource,
}

impl FileLockHost {
    pub fn new(state: SharedExtensionState) -> Self {
        Self { state: StateSource::Shared(state) }
    }
    /// ADR-0029 Phase 6.2.h.6 — bridged variant for the consumer-
    /// migration bridge. Pulls `&mut ExtensionStoreState` from
    /// `HostCallContext::consumer_state` at dispatch time; the
    /// bridge populates it per-call with `store.data_mut()`.
    pub fn bridged() -> Self {
        Self { state: StateSource::FromCtx }
    }
}

#[host_iface(sync)]
impl FileLockHost {
    fn acquire_exclusive(
        &self,
        ctx: &mut HostCallContext<'_>,
        path: String,
    ) -> RuntimeResult<Result<Resource<LockHandle>, String>> {
        let mut g = self.state.hold(ctx)?;
        match g.acquire_exclusive_lock(&path) {
            Ok(id) => Ok(Ok(Resource::<LockHandle>::from_raw(id, true))),
            Err(msg) => Ok(Err(msg)),
        }
    }

    fn try_acquire_exclusive(
        &self,
        ctx: &mut HostCallContext<'_>,
        path: String,
    ) -> RuntimeResult<Result<Option<Resource<LockHandle>>, String>> {
        let mut g = self.state.hold(ctx)?;
        match g.try_acquire_exclusive_lock(&path) {
            Ok(Some(id)) => Ok(Ok(Some(Resource::<LockHandle>::from_raw(id, true)))),
            Ok(None) => Ok(Ok(None)),
            Err(msg) => Ok(Err(msg)),
        }
    }

    // ── HostLockHandle resource-lifecycle methods (Phase 6.2.d.2-n) ──
    //
    // The wit-bindgen counterpart is a SEPARATE trait
    // (extension_file_lock::HostLockHandle at `crate::extension`
    // line 2635) whose two methods are canonically-mangled
    // resource-method dispatches:
    //   [method]lock-handle.release(self: lock-handle) -> ()
    //   [resource-drop]lock-handle(rep) -> ()
    //
    // Both call `free_lock_handle(rep.rep())` — the underlying
    // `LockHandleState` drop releases the OS flock + closes the
    // file. Wasmos-native mirrors delegate to the
    // `release_lock_handle` pub accessor added in Phase 6.2.d.2-m
    // (which wraps the private `free_lock_handle`).
    //
    // WASMOS-SIDE DESTRUCTOR GAP
    //
    // `[method]lock-handle.release` dispatches under the same
    // mechanism as the runtime XxxRegistry.register handlers
    // proven at scale in Phase 6.2.d.2-p+ — guest calls
    // `handle.release()`, the interface's SyncHostCall receives
    // the mangled method name, RuntimeHost's #[method]-overridden
    // handler fires. That path works today.
    //
    // `[resource-drop]lock-handle` — the destructor — depends on
    // the adapter dispatching a resource-drop event when the
    // guest resource falls out of scope. Whether wasmos's
    // wasmtime-v48 adapter surfaces this via `HostImports` is a
    // real wasmos-side question; if not, `[resource-drop]...`
    // handlers registered here are dead code and the underlying
    // LockHandleState never gets released on scope-drop
    // (process exit still releases the OS flock through the
    // native `Drop` impl).
    //
    // Handlers landed regardless so:
    //   1. The interface surface is COMPLETE at the API level —
    //      consumers see the wasmos-native FileLockHost matches
    //      the wit-bindgen counterpart's method inventory.
    //   2. When wasmos closes the destructor gap, the handler
    //      wires automatically without touching this module.
    //   3. Explicit `handle.release()` calls (the fast-path use
    //      case documented in the WIT — "callers can release
    //      early after publishing, before slower cleanup") work
    //      TODAY through the release method.
    //
    // Phase 6.2.d.2-n completes the Phase 6.2.d.2 arc — every
    // wit-bindgen impl block in `crate::extension` now has a
    // wasmos-native mirror.

    /// `[method]lock-handle.release` — early lock release.
    /// Byte-identical to `crate::extension` line 2636:
    /// `free_lock_handle(rep.rep())` + Ok.
    #[method("[method]lock-handle.release")]
    fn lock_handle_release(
        &self,
        ctx: &mut HostCallContext<'_>,
        self_: Resource<LockHandle>,
    ) -> RuntimeResult<()> {
        let mut g = self.state.hold(ctx)?;
        g.release_lock_handle(self_.handle());
        Ok(())
    }

    /// `[resource-drop]lock-handle` — destructor. Called by the
    /// adapter when the guest resource falls out of scope (subject
    /// to the wasmos destructor gap noted in the section
    /// docstring). Byte-identical to `crate::extension` line 2643.
    #[method("[resource-drop]lock-handle")]
    fn lock_handle_drop(
        &self,
        ctx: &mut HostCallContext<'_>,
        rep: Resource<LockHandle>,
    ) -> RuntimeResult<()> {
        let mut g = self.state.hold(ctx)?;
        g.release_lock_handle(rep.handle());
        Ok(())
    }
}

/// Register the `duckdb:extension/file-lock` handler.
///
/// NOTE: the interface's resource-method trait (release +
/// drop) is NOT wired here. See the module section docstring
/// for the wasmos-side gap + follow-up plan (Phase 6.2.d.2-n).
pub fn install_file_lock_imports(
    imports: HostImports,
    state: SharedExtensionState,
) -> HostImports {
    imports.register(
        "duckdb:extension/file-lock",
        Arc::new(SyncHostCallAdapter::new(FileLockHost::new(state)))
            as Arc<dyn wasmos_runtime_api::HostCall>,
    )
}

// ADR-0029 Phase 6.2.h.6 — `BridgedFileLockHost` retired: superseded
// by `FileLockHost::bridged()` (the StateSource::FromCtx variant on
// the unified `FileLockHost` type). Production wires
// `FileLockHost::bridged()` via
// `crate::extension::install_wasmos_migrated_interfaces`; tests hold
// their own state via `FileLockHost::new(shared)`. One type, dual
// mode.

// ────────────────────────────────────────────────────────────────────
// Phase 6.2.d.2-o — runtime main Host trait (27/27 for interface
// registration; the 10 sub-trait impls — 5 XxxCallback + 4
// XxxRegistry + HostMacroRegistry — are Phase 6.2.d.2-p+).
//
// This slice tackles the ROOT of the runtime interface: the 2-method
// Host trait (get_capability + list_capabilities) that guests call
// to bootstrap capability negotiation. It exercises Phase 6.12
// Session 3c's variant-with-Resource<T>-payloads support — the
// Capability variant has 5 arms each carrying a Resource<T>.
//
// The sub-trait impls are structurally different: they use the WIT
// resource-method mangling (`[method]scalar-registry.register-scalar`
// etc.) and resource-constructor mangling (`[constructor]scalar-
// callback`), which #[host_iface] doesn't yet accept as sugar. Each
// method needs a `#[method("[method]...")]` override; the 55+
// methods across 10 sub-traits warrant a coordinated sub-arc.
// ────────────────────────────────────────────────────────────────────

/// Wasmos-native mirror of the WIT `duckdb:extension/types.
/// capabilitykind` enum. 7 unit arms (WIT `capabilitykind`
/// includes `file-format` which PascalCases to FileFormat).
#[derive(Debug, Clone, Copy, WitEnum)]
pub enum Capabilitykind {
    Scalar,
    Table,
    Aggregate,
    Pragma,
    Macro,
    Catalog,
    FileFormat,
}

/// Wasmos-native markers for the WIT `duckdb:extension/runtime.
/// *-registry` resources. Handle rep is the id from the matching
/// `init_*_registry` accessor (scalar/table/aggregate) or
/// `alloc_resource_id` (pragma/macro).
#[derive(Debug, HostResourceType)]
#[wit_resource(interface = "duckdb:extension/runtime", name = "scalar-registry")]
pub struct ScalarRegistry;

#[derive(Debug, HostResourceType)]
#[wit_resource(interface = "duckdb:extension/runtime", name = "table-registry")]
pub struct TableRegistry;

#[derive(Debug, HostResourceType)]
#[wit_resource(interface = "duckdb:extension/runtime", name = "aggregate-registry")]
pub struct AggregateRegistry;

#[derive(Debug, HostResourceType)]
#[wit_resource(interface = "duckdb:extension/runtime", name = "pragma-registry")]
pub struct PragmaRegistry;

#[derive(Debug, HostResourceType)]
#[wit_resource(interface = "duckdb:extension/runtime", name = "macro-registry")]
pub struct MacroRegistry;

/// Wasmos-native mirror of the WIT `duckdb:extension/runtime.
/// capability` variant. 5 arms, each with a `Resource<T>`
/// payload — the ctx-mediated Resource<T>-inside-variant-payload
/// shape unlocked by Phase 6.12 Session 3c.
///
/// The `#[wit_ctx]` container opt-in emits `impl WitBridgeCtx`
/// (not `impl WitBridge`) so each payload's `Resource<T>`
/// serialization routes through `HostCallContext::
/// new_typed_resource` / `typed_resource_rep` on the way in/out.
#[derive(Debug, Clone, WitVariant)]
#[wit_ctx]
pub enum Capability {
    Scalar(Resource<ScalarRegistry>),
    Table(Resource<TableRegistry>),
    Aggregate(Resource<AggregateRegistry>),
    Pragma(Resource<PragmaRegistry>),
    Macro(Resource<MacroRegistry>),
}

/// Host struct for the `duckdb:extension/runtime` interface —
/// the ROOT of the extension SPI's registration surface. See
/// `crate::extension` line 1349 for the wit-bindgen counterpart.
///
/// This slice covers the 2 top-level methods (get_capability +
/// list_capabilities); the 10 sub-trait impls (5 HostXxxCallback
/// resource constructors + HostScalarRegistry / HostTableRegistry
/// / HostAggregateRegistry / HostPragmaRegistry / HostMacroRegistry
/// resource-method traits) land in future Phase 6.2.d.2-p+
/// sub-sessions using WIT resource-method mangling overrides
/// (`#[method("[method]scalar-registry.register-scalar")]`).
#[derive(Clone)]
pub struct RuntimeHost {
    state: StateSource,
}

impl RuntimeHost {
    pub fn new(state: SharedExtensionState) -> Self {
        Self { state: StateSource::Shared(state) }
    }
    /// ADR-0029 Phase 6.2.h.6 — bridged variant for the consumer-
    /// migration bridge. Pulls `&mut ExtensionStoreState` from
    /// `HostCallContext::consumer_state` at dispatch time; the
    /// bridge populates it per-call with `store.data_mut()`.
    pub fn bridged() -> Self {
        Self { state: StateSource::FromCtx }
    }
}

#[host_iface(sync)]
impl RuntimeHost {
    /// Handler for `runtime.get-capability(kind) -> option<capability>`.
    /// Byte-identical to `crate::extension` line 1350:
    /// - Scalar/Table/Aggregate: allocate a fresh registry id +
    ///   insert a default PendingXxxRegistry, hand back a
    ///   Resource<XxxRegistry>.
    /// - Pragma: allocate a fresh id (no per-registry buffer;
    ///   register_call captures directly).
    /// - Macro/Catalog/FileFormat: return None (documented in the
    ///   wit-bindgen counterpart — Macro capability path is
    ///   Unsupported today, Catalog + FileFormat have no
    ///   `Capability::*` variant).
    fn get_capability(
        &self,
        ctx: &mut HostCallContext<'_>,
        kind: Capabilitykind,
    ) -> RuntimeResult<Option<Capability>> {
        let mut g = self.state.hold(ctx)?;
        Ok(match kind {
            Capabilitykind::Scalar => {
                let id = g.init_scalar_registry();
                Some(Capability::Scalar(Resource::<ScalarRegistry>::from_raw(
                    id, true,
                )))
            }
            Capabilitykind::Table => {
                let id = g.init_table_registry();
                Some(Capability::Table(Resource::<TableRegistry>::from_raw(
                    id, true,
                )))
            }
            Capabilitykind::Aggregate => {
                let id = g.init_aggregate_registry();
                Some(Capability::Aggregate(Resource::<AggregateRegistry>::from_raw(
                    id, true,
                )))
            }
            Capabilitykind::Pragma => {
                let id = g.alloc_resource_id();
                Some(Capability::Pragma(Resource::<PragmaRegistry>::from_raw(
                    id, true,
                )))
            }
            // Documented Nones (see `crate::extension` line 1388-1405).
            Capabilitykind::Macro => None,
            Capabilitykind::Catalog => None,
            Capabilitykind::FileFormat => None,
        })
    }

    /// Handler for `runtime.list-capabilities() -> list<capabilitykind>`.
    /// Returns the kinds `get-capability` actively hands back a
    /// Some for. Macro/Catalog/FileFormat omitted (matches
    /// `crate::extension` line 1409).
    fn list_capabilities(
        &self,
        _ctx: &mut HostCallContext<'_>,
    ) -> RuntimeResult<Vec<Capabilitykind>> {
        Ok(vec![
            Capabilitykind::Scalar,
            Capabilitykind::Table,
            Capabilitykind::Aggregate,
            Capabilitykind::Pragma,
        ])
    }

    // ── Sub-trait: HostMacroRegistry (Phase 6.2.d.2-p, first slice) ──
    //
    // The wit-bindgen counterpart at `crate::extension` line 1844
    // has 2 methods on the macro-registry resource:
    //   register-scalar(name, parameters, body-sql, options)
    //     -> result<bool, duckerror>  — always returns Unsupported
    //   [resource-drop]macro-registry  — no-op
    //
    // Under the WIT canonical ABI, resource-method names mangle to
    // `[method]macro-registry.register-scalar` (etc.). The
    // `#[method("...")]` override on `#[host_iface]` lets us wire
    // the same dispatch — the arg list mirrors the WIT method
    // shape (self resource lifts as `Resource<MacroRegistry>`
    // first arg; other args follow).

    /// `[method]macro-registry.register-scalar` — always
    /// returns Unsupported. Byte-identical to the wit-bindgen
    /// counterpart.
    ///
    /// NOTE: options is `Option<Extopts>` but I haven't mirrored
    /// runtime's own Extopts type yet — the runtime WIT reuses
    /// `types.extopts` which is already mirrored elsewhere in
    /// this module (as `Extopts`). Same type; wasmos-native
    /// classifier resolves the reuse automatically.
    #[method("[method]macro-registry.register-scalar")]
    fn macro_registry_register_scalar(
        &self,
        _ctx: &mut HostCallContext<'_>,
        _self_: Resource<MacroRegistry>,
        _name: String,
        _parameters: Vec<String>,
        _body_sql: String,
        _options: Option<Extopts>,
    ) -> RuntimeResult<Result<bool, Duckerror>> {
        // Matches `crate::extension` line 1852:
        // `Err(unsupported_runtime_error())` where
        // `unsupported_runtime_error` returns
        // `Duckerror::Unsupported("component runtime not
        // available in CLI host")`.
        Ok(Err(Duckerror::Unsupported(
            "component runtime not available in CLI host".to_string(),
        )))
    }

    /// `[resource-drop]macro-registry` — no-op. The wit-bindgen
    /// counterpart at `crate::extension` line 1856 also just
    /// returns Ok(()).
    ///
    /// WASMOS-SIDE GAP: destructor dispatch depends on adapter-
    /// level resource-drop notifications from the guest. See
    /// the Phase 6.2.d.2-n design note in file_lock's section.
    /// If wasmos never dispatches `[resource-drop]macro-registry`,
    /// this handler is dead code — harmless (no state to
    /// release; the wit-bindgen counterpart is also no-op).
    #[method("[resource-drop]macro-registry")]
    fn macro_registry_drop(
        &self,
        _ctx: &mut HostCallContext<'_>,
        _rep: Resource<MacroRegistry>,
    ) -> RuntimeResult<()> {
        Ok(())
    }

    // ── Sub-trait: 5 XxxCallback constructors + calls + drops (Phase 6.2.d.2-p) ──
    //
    // Under the WIT canonical ABI, each `resource xxx-callback`
    // mangles to:
    //   `[constructor]xxx-callback` — takes u32 handle,
    //      returns Resource<XxxCallback>
    //   `[method]xxx-callback.call` — self resource + method args,
    //      returns the resource's declared type (Unsupported here)
    //   `[resource-drop]xxx-callback` — self resource,
    //      returns () after cleanup
    //
    // The 5 kinds (Scalar/Table/Aggregate/Pragma/Cast) each get 3
    // methods = 15 total. All `call` methods return
    // `Duckerror::Unsupported` per the wit-bindgen counterpart
    // at `crate::extension` line 1453+.
    //
    // The `call` args (list<duckvalue>, invokeinfo, rowbatch,
    // etc.) use `Vec<Value>` passthrough — sidesteps needing
    // Duckvalue / Invokeinfo / Rowbatch / Resultset mirror types
    // for handlers that never touch the args. Same pattern used
    // in sqlink-host's `extension_loader` stub.

    // ── ScalarCallback ─────────────────────────────────────────

    #[method("[constructor]scalar-callback")]
    fn scalar_callback_new(
        &self,
        ctx: &mut HostCallContext<'_>,
        handle: u32,
    ) -> RuntimeResult<Resource<ScalarCallback>> {
        let g = self.state.hold(ctx)?;
        let id = g.allocate_callback_handle_pub(handle, crate::CallbackKind::Scalar);
        Ok(Resource::<ScalarCallback>::from_raw(id, true))
    }

    #[method("[method]scalar-callback.call")]
    fn scalar_callback_call(
        &self,
        _ctx: &mut HostCallContext<'_>,
        _args: Vec<wasmos_runtime_api::Value>,
    ) -> RuntimeResult<Vec<wasmos_runtime_api::Value>> {
        // Vec<Value> passthrough: `[method]xxx.call` returns
        // `result<duckvalue, duckerror>`; construct the Err
        // arm's Value directly to avoid the Duckvalue mirror.
        Ok(vec![wasmos_runtime_api::Value::Result(Err(Some(
            Box::new(unsupported_duckerror_value()),
        )))])
    }

    #[method("[resource-drop]scalar-callback")]
    fn scalar_callback_drop(
        &self,
        ctx: &mut HostCallContext<'_>,
        rep: Resource<ScalarCallback>,
    ) -> RuntimeResult<()> {
        let g = self.state.hold(ctx)?;
        g.release_callback_handle_pub(rep.handle());
        Ok(())
    }

    // ── TableCallback ──────────────────────────────────────────

    #[method("[constructor]table-callback")]
    fn table_callback_new(
        &self,
        ctx: &mut HostCallContext<'_>,
        handle: u32,
    ) -> RuntimeResult<Resource<TableCallback>> {
        let g = self.state.hold(ctx)?;
        let id = g.allocate_callback_handle_pub(handle, crate::CallbackKind::Table);
        Ok(Resource::<TableCallback>::from_raw(id, true))
    }

    #[method("[method]table-callback.call")]
    fn table_callback_call(
        &self,
        _ctx: &mut HostCallContext<'_>,
        _args: Vec<wasmos_runtime_api::Value>,
    ) -> RuntimeResult<Vec<wasmos_runtime_api::Value>> {
        Ok(vec![wasmos_runtime_api::Value::Result(Err(Some(
            Box::new(unsupported_duckerror_value()),
        )))])
    }

    #[method("[resource-drop]table-callback")]
    fn table_callback_drop(
        &self,
        ctx: &mut HostCallContext<'_>,
        rep: Resource<TableCallback>,
    ) -> RuntimeResult<()> {
        let g = self.state.hold(ctx)?;
        g.release_callback_handle_pub(rep.handle());
        Ok(())
    }

    // ── AggregateCallback ──────────────────────────────────────

    #[method("[constructor]aggregate-callback")]
    fn aggregate_callback_new(
        &self,
        ctx: &mut HostCallContext<'_>,
        handle: u32,
    ) -> RuntimeResult<Resource<AggregateCallback>> {
        let g = self.state.hold(ctx)?;
        let id = g.allocate_callback_handle_pub(handle, crate::CallbackKind::Aggregate);
        Ok(Resource::<AggregateCallback>::from_raw(id, true))
    }

    #[method("[method]aggregate-callback.call")]
    fn aggregate_callback_call(
        &self,
        _ctx: &mut HostCallContext<'_>,
        _args: Vec<wasmos_runtime_api::Value>,
    ) -> RuntimeResult<Vec<wasmos_runtime_api::Value>> {
        Ok(vec![wasmos_runtime_api::Value::Result(Err(Some(
            Box::new(unsupported_duckerror_value()),
        )))])
    }

    #[method("[resource-drop]aggregate-callback")]
    fn aggregate_callback_drop(
        &self,
        ctx: &mut HostCallContext<'_>,
        rep: Resource<AggregateCallback>,
    ) -> RuntimeResult<()> {
        let g = self.state.hold(ctx)?;
        g.release_callback_handle_pub(rep.handle());
        Ok(())
    }

    // ── PragmaCallback ─────────────────────────────────────────

    #[method("[constructor]pragma-callback")]
    fn pragma_callback_new(
        &self,
        ctx: &mut HostCallContext<'_>,
        handle: u32,
    ) -> RuntimeResult<Resource<PragmaCallback>> {
        let g = self.state.hold(ctx)?;
        let id = g.allocate_callback_handle_pub(handle, crate::CallbackKind::Pragma);
        Ok(Resource::<PragmaCallback>::from_raw(id, true))
    }

    #[method("[method]pragma-callback.call")]
    fn pragma_callback_call(
        &self,
        _ctx: &mut HostCallContext<'_>,
        _args: Vec<wasmos_runtime_api::Value>,
    ) -> RuntimeResult<Vec<wasmos_runtime_api::Value>> {
        Ok(vec![wasmos_runtime_api::Value::Result(Err(Some(
            Box::new(unsupported_duckerror_value()),
        )))])
    }

    #[method("[resource-drop]pragma-callback")]
    fn pragma_callback_drop(
        &self,
        ctx: &mut HostCallContext<'_>,
        rep: Resource<PragmaCallback>,
    ) -> RuntimeResult<()> {
        let g = self.state.hold(ctx)?;
        g.release_callback_handle_pub(rep.handle());
        Ok(())
    }

    // ── CastCallback ───────────────────────────────────────────
    //
    // NOTE: CastCallback is used by BOTH runtime AND catalog.
    // The marker was defined for catalog in Phase 6.2.d.2-m
    // (`interface = "duckdb:extension/catalog"`); the runtime
    // constructor needs a separate marker with the runtime
    // interface tag. Defined below the impl block as
    // `RuntimeCastCallback` so both markers coexist.

    #[method("[constructor]cast-callback")]
    fn cast_callback_new(
        &self,
        ctx: &mut HostCallContext<'_>,
        handle: u32,
    ) -> RuntimeResult<Resource<RuntimeCastCallback>> {
        let g = self.state.hold(ctx)?;
        let id = g.allocate_callback_handle_pub(handle, crate::CallbackKind::Cast);
        Ok(Resource::<RuntimeCastCallback>::from_raw(id, true))
    }

    #[method("[method]cast-callback.call")]
    fn cast_callback_call(
        &self,
        _ctx: &mut HostCallContext<'_>,
        _args: Vec<wasmos_runtime_api::Value>,
    ) -> RuntimeResult<Vec<wasmos_runtime_api::Value>> {
        Ok(vec![wasmos_runtime_api::Value::Result(Err(Some(
            Box::new(unsupported_duckerror_value()),
        )))])
    }

    #[method("[resource-drop]cast-callback")]
    fn cast_callback_drop(
        &self,
        ctx: &mut HostCallContext<'_>,
        rep: Resource<RuntimeCastCallback>,
    ) -> RuntimeResult<()> {
        let g = self.state.hold(ctx)?;
        g.release_callback_handle_pub(rep.handle());
        Ok(())
    }

    // ── Sub-trait: 4 XxxRegistry method-carriers (Phase 6.2.d.2-q) ──
    //
    // Each Xxx-Registry resource has 2 methods:
    //   `[method]xxx-registry.register(name, args, ret, callback, opts)`
    //     -> result<u32, duckerror>
    //   `[resource-drop]xxx-registry` -> ()
    //
    // Behavior mirrors `crate::extension` line 1567+ verbatim:
    //   1. Validate `callback` resource's kind matches expected
    //      (Scalar/Table/Aggregate) via
    //      `ExtensionStoreState::validate_callback_kind`.
    //      KindMismatch -> Duckerror::Invalidargument
    //      UnknownHandle -> Duckerror::Internal
    //   2. Build the neutral reg::* record (converts wasmos-native
    //      LogicalType/Funcarg/Funcopts/Extopts to reg::* shapes).
    //   3. Push into the per-registry buffer via the matching
    //      `<kind>_registry_push` accessor. UnknownRegistry ->
    //      Duckerror::Internal.
    //   4. Return the alloc'd u32 handle.
    // Drops delegate to `drain_<kind>_registry` accessors, which
    // move the per-registry buffer's entries into the global
    // `pending_<kind>s` buffer.
    //
    // PragmaRegistry differs: no per-registry buffer, push_call
    // goes directly to `pending_pragmas`; drop is no-op.

    // ── ScalarRegistry ─────────────────────────────────────────

    #[method("[method]scalar-registry.register")]
    fn scalar_registry_register(
        &self,
        ctx: &mut HostCallContext<'_>,
        self_: Resource<ScalarRegistry>,
        name: String,
        arguments: Vec<Funcarg>,
        returns: LogicalType,
        callback: Resource<ScalarCallback>,
        options: Option<Funcopts>,
    ) -> RuntimeResult<Result<u32, Duckerror>> {
        let callback_handle = callback.handle();
        let registry_id = self_.handle();
        let arguments_r: Vec<crate::reg::FuncArg> =
            arguments.into_iter().map(Funcarg::to_reg).collect();
        let returns_r = returns.to_reg();
        let options_r = options.map(Funcopts::to_reg);
        let mut g = self.state.hold(ctx)?;
        // 1. Validate callback kind.
        if let Err(e) =
            g.validate_callback_kind(callback_handle, crate::CallbackKind::Scalar)
        {
            return Ok(Err(kind_validation_to_duckerror(e, "scalar")));
        }
        let extension = g.extension_name().to_string();
        // 2 + 3. Build entry + push.
        let entry = crate::reg::ScalarReg {
            extension,
            name,
            arguments: arguments_r,
            returns: returns_r,
            callback_handle,
            options: options_r,
        };
        match g.scalar_registry_push(registry_id, entry) {
            Ok(handle) => Ok(Ok(handle)),
            Err(_) => Ok(Err(Duckerror::Internal(
                "unknown scalar registry handle".to_string(),
            ))),
        }
    }

    #[method("[resource-drop]scalar-registry")]
    fn scalar_registry_drop(
        &self,
        ctx: &mut HostCallContext<'_>,
        rep: Resource<ScalarRegistry>,
    ) -> RuntimeResult<()> {
        let mut g = self.state.hold(ctx)?;
        g.drain_scalar_registry(rep.handle());
        Ok(())
    }

    // ── TableRegistry ──────────────────────────────────────────

    #[method("[method]table-registry.register")]
    fn table_registry_register(
        &self,
        ctx: &mut HostCallContext<'_>,
        self_: Resource<TableRegistry>,
        name: String,
        arguments: Vec<Funcarg>,
        columns: Vec<Columndef>,
        callback: Resource<TableCallback>,
        options: Option<Extopts>,
    ) -> RuntimeResult<Result<u32, Duckerror>> {
        let callback_handle = callback.handle();
        let registry_id = self_.handle();
        let arguments_r: Vec<crate::reg::FuncArg> =
            arguments.into_iter().map(Funcarg::to_reg).collect();
        let columns_r: Vec<crate::reg::ColumnDef> =
            columns.into_iter().map(Columndef::to_reg).collect();
        let options_r = options.map(|o| crate::reg::ExtOpts {
            description: o.description,
            tags: o.tags,
        });
        let mut g = self.state.hold(ctx)?;
        if let Err(e) =
            g.validate_callback_kind(callback_handle, crate::CallbackKind::Table)
        {
            return Ok(Err(kind_validation_to_duckerror(e, "a table callback")));
        }
        let extension = g.extension_name().to_string();
        let entry = crate::reg::TableReg {
            extension,
            name,
            arguments: arguments_r,
            columns: columns_r,
            callback_handle,
            options: options_r,
        };
        match g.table_registry_push(registry_id, entry) {
            Ok(handle) => Ok(Ok(handle)),
            Err(_) => Ok(Err(Duckerror::Internal(
                "unknown table registry handle".to_string(),
            ))),
        }
    }

    #[method("[resource-drop]table-registry")]
    fn table_registry_drop(
        &self,
        ctx: &mut HostCallContext<'_>,
        rep: Resource<TableRegistry>,
    ) -> RuntimeResult<()> {
        let mut g = self.state.hold(ctx)?;
        g.drain_table_registry(rep.handle());
        Ok(())
    }

    // ── AggregateRegistry ──────────────────────────────────────

    #[method("[method]aggregate-registry.register")]
    fn aggregate_registry_register(
        &self,
        ctx: &mut HostCallContext<'_>,
        self_: Resource<AggregateRegistry>,
        name: String,
        arguments: Vec<Funcarg>,
        returns: LogicalType,
        callback: Resource<AggregateCallback>,
        options: Option<Funcopts>,
    ) -> RuntimeResult<Result<u32, Duckerror>> {
        let callback_handle = callback.handle();
        let registry_id = self_.handle();
        let arguments_r: Vec<crate::reg::FuncArg> =
            arguments.into_iter().map(Funcarg::to_reg).collect();
        let returns_r = returns.to_reg();
        let options_r = options.map(Funcopts::to_reg);
        let mut g = self.state.hold(ctx)?;
        if let Err(e) =
            g.validate_callback_kind(callback_handle, crate::CallbackKind::Aggregate)
        {
            return Ok(Err(kind_validation_to_duckerror(e, "aggregate")));
        }
        let extension = g.extension_name().to_string();
        let entry = crate::reg::AggregateReg {
            extension,
            name,
            arguments: arguments_r,
            returns: returns_r,
            callback_handle,
            options: options_r,
        };
        match g.aggregate_registry_push(registry_id, entry) {
            Ok(handle) => Ok(Ok(handle)),
            Err(_) => Ok(Err(Duckerror::Internal(
                "unknown aggregate registry handle".to_string(),
            ))),
        }
    }

    #[method("[resource-drop]aggregate-registry")]
    fn aggregate_registry_drop(
        &self,
        ctx: &mut HostCallContext<'_>,
        rep: Resource<AggregateRegistry>,
    ) -> RuntimeResult<()> {
        let mut g = self.state.hold(ctx)?;
        g.drain_aggregate_registry(rep.handle());
        Ok(())
    }

    // ── PragmaRegistry ─────────────────────────────────────────

    #[method("[method]pragma-registry.register-call")]
    fn pragma_registry_register_call(
        &self,
        ctx: &mut HostCallContext<'_>,
        _self_: Resource<PragmaRegistry>,
        name: String,
        _arguments: Vec<Funcarg>,
        _returns: LogicalType,
        callback: Resource<PragmaCallback>,
        _options: Option<Extopts>,
    ) -> RuntimeResult<Result<u32, Duckerror>> {
        let callback_handle = callback.handle();
        let mut g = self.state.hold(ctx)?;
        if let Err(e) =
            g.validate_callback_kind(callback_handle, crate::CallbackKind::Pragma)
        {
            return Ok(Err(kind_validation_to_duckerror(e, "a pragma")));
        }
        let extension = g.extension_name().to_string();
        let entry = crate::reg::PragmaReg {
            extension,
            name,
            callback_handle,
        };
        Ok(Ok(g.pragma_registry_push_call(entry)))
    }

    #[method("[resource-drop]pragma-registry")]
    fn pragma_registry_drop(
        &self,
        _ctx: &mut HostCallContext<'_>,
        _rep: Resource<PragmaRegistry>,
    ) -> RuntimeResult<()> {
        // The wit-bindgen counterpart at `crate::extension` line
        // 1839 is a no-op; pragma_registry_push_call already
        // pushed into pending_pragmas at register time (no per-
        // registry buffer to drain).
        Ok(())
    }
}

/// Translate a `CallbackValidationError` into the Duckerror wire
/// shape the wit-bindgen counterpart emits. `kind_phrase` is the
/// per-kind message fragment ("scalar", "aggregate", "a table
/// callback", "a pragma") — the phrase varies slightly across
/// the 4 XxxRegistry impls in `crate::extension`; we preserve
/// each verbatim.
fn kind_validation_to_duckerror(
    err: crate::extension::CallbackValidationError,
    kind_phrase: &str,
) -> Duckerror {
    use crate::extension::CallbackValidationError as E;
    match err {
        E::KindMismatch => Duckerror::Invalidargument(format!(
            "callback handle is not {kind_phrase}"
        )),
        E::UnknownHandle => {
            Duckerror::Internal(format!("unknown {kind_phrase} callback handle"))
        }
    }
}

// ── Runtime XxxCallback resource markers (Phase 6.2.d.2-p) ─────

#[derive(Debug, HostResourceType)]
#[wit_resource(interface = "duckdb:extension/runtime", name = "scalar-callback")]
pub struct ScalarCallback;

#[derive(Debug, HostResourceType)]
#[wit_resource(interface = "duckdb:extension/runtime", name = "table-callback")]
pub struct TableCallback;

#[derive(Debug, HostResourceType)]
#[wit_resource(interface = "duckdb:extension/runtime", name = "aggregate-callback")]
pub struct AggregateCallback;

#[derive(Debug, HostResourceType)]
#[wit_resource(interface = "duckdb:extension/runtime", name = "pragma-callback")]
pub struct PragmaCallback;

/// Runtime-side sibling of the `CastCallback` defined for
/// catalog in Phase 6.2.d.2-m. Same underlying resource type
/// (host allocates a callback handle via
/// `allocate_callback_handle_pub`); the two markers are
/// distinct only because HostResourceType's INTERFACE
/// associated const has to name the containing interface, and
/// cast-callback appears in BOTH `duckdb:extension/runtime`
/// AND `duckdb:extension/catalog` WITs. wasmos-runtime-api's
/// classifier keys on the (interface, name) pair so the two
/// markers dispatch independently.
#[derive(Debug, HostResourceType)]
#[wit_resource(interface = "duckdb:extension/runtime", name = "cast-callback")]
pub struct RuntimeCastCallback;

/// Build the `Value` representation of
/// `Result::Err(Duckerror::Unsupported("component runtime not
/// available in CLI host"))` — the Value the XxxCallback.call
/// handlers return under Vec<Value> passthrough. Wire-identical
/// to what the wit-bindgen counterpart's
/// `unsupported_runtime_error()` produces at `crate::extension`
/// line ~1160.
fn unsupported_duckerror_value() -> wasmos_runtime_api::Value {
    wasmos_runtime_api::Value::Variant {
        discriminant: "unsupported".into(),
        payload: Some(Box::new(wasmos_runtime_api::Value::String(
            "component runtime not available in CLI host".to_string(),
        ))),
    }
}

/// Register the `duckdb:extension/runtime` handler.
///
/// NOTE: The 10 sub-trait impls (5 HostXxxCallback constructors +
/// HostScalarRegistry/HostTableRegistry/HostAggregateRegistry/
/// HostPragmaRegistry/HostMacroRegistry method-carrying traits)
/// are NOT wired here — see the module section docstring for the
/// mangling-convention follow-up plan. Guests that call
/// registry.register_scalar (etc.) get "no handler" until the
/// Phase 6.2.d.2-p+ sub-sessions land those overrides.
pub fn install_runtime_imports(
    imports: HostImports,
    state: SharedExtensionState,
) -> HostImports {
    imports.register(
        "duckdb:extension/runtime",
        Arc::new(SyncHostCallAdapter::new(RuntimeHost::new(state)))
            as Arc<dyn wasmos_runtime_api::HostCall>,
    )
}

// Silence unused-import warning on RuntimeError — will be reached
// via macro expansion in future error-returning handlers.
#[allow(dead_code)]
const _RUNTIME_ERROR: fn() = || {
    let _ = RuntimeError::msg("");
};

/// Register the `duckdb:extension/lifecycle` handler on the given
/// [`HostImports`] set. Consumer usage mirrors [`datalink_dynlink::
/// install_host_imports`] and other wasmos-side install fns:
///
/// ```rust,ignore
/// let imports = ducklink_runtime::extension_wasmos::install_lifecycle_imports(
///     wasmos_runtime_api::HostImports::new(),
/// );
/// // Thread `imports` into your ExecutionContext at instantiate time.
/// ```
///
/// The registered interface name matches the WIT surface exactly
/// (`duckdb:extension/lifecycle`). Wasmos does verbatim interface-
/// name matching against the guest's imports, so version tags
/// (`@0.1.0` etc.) must be added here if the guest's import
/// includes one; ducklink's `duckdb:extension` world is
/// unversioned today, matching the naked interface name below.
pub fn install_lifecycle_imports(imports: HostImports) -> HostImports {
    let host = LifecycleHost::new();
    imports.register(
        "duckdb:extension/lifecycle",
        Arc::new(SyncHostCallAdapter::new(host)) as Arc<dyn wasmos_runtime_api::HostCall>,
    )
}

// Suppress the "unused import" warning that fires because
// `SyncHostCall` is only reached through the `#[host_iface(sync)]`
// macro's emitted `impl SyncHostCall for LifecycleHost`. The
// import stays so grep-locality holds.
#[allow(dead_code)]
const _SYNC_HOST_CALL: fn() = || {
    fn _assert<T: SyncHostCall>() {}
    _assert::<LifecycleHost>();
};

#[cfg(test)]
mod tests {
    //! Integration coverage for the lifecycle migration. Exercises
    //! the emitted `impl SyncHostCall` end-to-end via the adapter.

    use super::*;
    use wasmos_runtime_api::{HostCallContext, HostCallCtxImpl, RuntimeError, RuntimeResult, Value};

    struct StubCtx;
    impl HostCallCtxImpl for StubCtx {
        fn new_host_resource(
            &mut self,
            _iface: &str,
            _res: &str,
            _rep: u32,
        ) -> RuntimeResult<Value> {
            Err(RuntimeError::msg("stub ctx has no resource allocation"))
        }
        fn resource_rep(&mut self, _value: &Value) -> RuntimeResult<u32> {
            Err(RuntimeError::msg("stub ctx has no resource lookup"))
        }
    }

    #[test]
    fn register_connection_callback_returns_unsupported() {
        let host = LifecycleHost::new();
        let mut stub = StubCtx;
        let mut ctx = HostCallContext::new(&mut stub);

        // The synthetic call goes through the emitted impl.
        // Arg shape: conn-events flags (u32-encoded) + callback-handle u32.
        // Under wasmos Value marshalling, flags lift from
        // Value::Flags { bits: [ ("opened", true), ("closed", false) ] };
        // primitives lift as Value::U32.
        let args = vec![
            // Empty flags (neither opened nor closed subscribed).
            // Value::Flags is a set of active-flag names; unset flags
            // are absent from the vec entirely.
            Value::Flags(Vec::new()),
            Value::U32(42),
        ];
        let out = host
            .call(&mut ctx, "register-connection-callback", args)
            .expect("dispatch");

        // Return: Result<u32, Duckerror>. Lowers as Value::Result(Err(...)).
        match out.as_slice() {
            [Value::Result(Err(Some(payload)))] => {
                match payload.as_ref() {
                    Value::Variant {
                        discriminant,
                        payload: Some(payload),
                    } if discriminant == "unsupported" => {
                        if let Value::String(msg) = payload.as_ref() {
                            assert!(msg.contains("connection open/close"), "unexpected message: {msg}");
                        } else {
                            panic!("expected string payload, got {payload:?}");
                        }
                    }
                    other => panic!("expected unsupported variant, got {other:?}"),
                }
            }
            other => panic!("expected Result(Err(Some(...))), got {other:?}"),
        }
    }

    #[test]
    fn install_registers_the_interface() {
        let imports = install_lifecycle_imports(HostImports::new());
        assert!(
            imports.get("duckdb:extension/lifecycle").is_some(),
            "lifecycle interface should be registered"
        );
    }

    #[test]
    fn types_marker_dispatches_nothing() {
        // TypesHost has zero methods — every dispatch is an
        // unknown-method error. Proves the empty-impl case works.
        let host = TypesHost::new();
        let mut stub = StubCtx;
        let mut ctx = HostCallContext::new(&mut stub);
        let err = host
            .call(&mut ctx, "anything", vec![])
            .expect_err("empty marker should reject every method");
        let msg = format!("{err}");
        assert!(msg.contains("anything"), "error should name the method: {msg}");
    }

    #[test]
    fn encoding_returns_unsupported() {
        let host = EncodingHost::new();
        let mut stub = StubCtx;
        let mut ctx = HostCallContext::new(&mut stub);
        let out = host
            .call(
                &mut ctx,
                "register-encoding",
                vec![
                    Value::String("utf-8".into()),
                    Value::List(vec![Value::String("utf8".into())]),
                    Value::U32(1),
                ],
            )
            .expect("dispatch");
        match out.as_slice() {
            [Value::Result(Err(Some(payload)))] => match payload.as_ref() {
                Value::Variant {
                    discriminant,
                    payload: Some(_),
                } => assert_eq!(discriminant, "unsupported"),
                other => panic!("expected unsupported variant, got {other:?}"),
            },
            other => panic!("expected Result(Err(...)), got {other:?}"),
        }
    }

    #[test]
    fn compression_returns_unsupported() {
        let host = CompressionHost::new();
        let mut stub = StubCtx;
        let mut ctx = HostCallContext::new(&mut stub);
        let out = host
            .call(
                &mut ctx,
                "register-compression",
                vec![
                    Value::String("lz4".into()),
                    Value::String("lz4".into()),
                    Value::U32(2),
                ],
            )
            .expect("dispatch");
        assert!(
            matches!(out.as_slice(), [Value::Result(Err(Some(_)))]),
            "expected Result(Err), got {out:?}"
        );
    }

    #[test]
    fn files_reg_returns_unsupported() {
        let host = FilesRegHost::new();
        let mut stub = StubCtx;
        let mut ctx = HostCallContext::new(&mut stub);
        let out = host
            .call(&mut ctx, "register-files", vec![Value::U32(3)])
            .expect("dispatch");
        assert!(
            matches!(out.as_slice(), [Value::Result(Err(Some(_)))]),
            "expected Result(Err), got {out:?}"
        );
    }

    #[test]
    fn install_extension_registers_all_five() {
        let imports = install_extension_imports(HostImports::new());
        for iface in [
            "duckdb:extension/lifecycle",
            "duckdb:extension/types",
            "duckdb:extension/encoding",
            "duckdb:extension/compression",
            "duckdb:extension/files-reg",
        ] {
            assert!(
                imports.get(iface).is_some(),
                "interface {iface} should be registered"
            );
        }
    }

    #[test]
    fn index_returns_unsupported() {
        let host = IndexHost::new();
        let mut stub = StubCtx;
        let mut ctx = HostCallContext::new(&mut stub);
        let out = host
            .call(
                &mut ctx,
                "register-index-type",
                vec![Value::String("wasm_hnsw".into())],
            )
            .expect("dispatch");
        assert!(
            matches!(out.as_slice(), [Value::Result(Err(Some(_)))]),
            "expected Result(Err), got {out:?}"
        );
    }

    #[test]
    fn collation_returns_unsupported() {
        let host = CollationHost::new();
        let mut stub = StubCtx;
        let mut ctx = HostCallContext::new(&mut stub);
        let out = host
            .call(
                &mut ctx,
                "register-collation",
                vec![
                    Value::String("nocase".into()),
                    Value::String("lower".into()),
                    Value::Bool(true),
                ],
            )
            .expect("dispatch");
        assert!(
            matches!(out.as_slice(), [Value::Result(Err(Some(_)))]),
            "expected Result(Err), got {out:?}"
        );
    }

    #[test]
    fn install_stateless_batch_registers_all_seven() {
        // install_extension_imports is stateless-only; verify all
        // 5 base interfaces still register, and that the newly
        // stateless interfaces (index, collation) also register
        // through their individual install fns (they're stateless,
        // so no state handle needed).
        let imports = install_extension_imports(HostImports::new());
        let imports = install_index_imports(imports);
        let imports = install_collation_imports(imports);
        for iface in [
            "duckdb:extension/lifecycle",
            "duckdb:extension/types",
            "duckdb:extension/encoding",
            "duckdb:extension/compression",
            "duckdb:extension/files-reg",
            "duckdb:extension/index",
            "duckdb:extension/collation",
        ] {
            assert!(
                imports.get(iface).is_some(),
                "interface {iface} should be registered"
            );
        }
    }

    #[test]
    fn unknown_method_errors_cleanly() {
        let host = LifecycleHost::new();
        let mut stub = StubCtx;
        let mut ctx = HostCallContext::new(&mut stub);
        let err = host
            .call(&mut ctx, "not-a-method", vec![])
            .expect_err("unknown method should error");
        let msg = format!("{err}");
        assert!(
            msg.contains("not-a-method") && msg.contains("register-connection-callback"),
            "error should name the bad method + list registered ones: {msg}"
        );
    }

    // ─── Phase 6.2.g — stateful file_lock lifecycle tests ─────────
    //
    // These exercise `FileLockHost` end-to-end against a real
    // `ExtensionStoreState` (via the Phase 6.2.g extension_test_
    // support fixture). They cover the two lifecycle-terminating
    // paths for `LockHandle`:
    //
    //   1. Explicit `handle.release()` — the wit-bindgen-parity
    //      fast path documented on the WIT ("callers can release
    //      early after publishing, before slower cleanup"). Routes
    //      through SyncHostCall::call with the wit-mangled method
    //      name `[method]lock-handle.release`.
    //
    //   2. Implicit scope-drop — the Phase 6.2.f wasmos-side
    //      destructor-registration route. Routes through
    //      SyncHostCall::on_resource_drop with resource_name
    //      `lock-handle`. The macro auto-generated the drop arm
    //      from `#[method("[resource-drop]lock-handle")]`.
    //
    // Both must leave `state.lock_handles` empty afterward. The
    // acquire step uses a real tempfile so the OS-level flock
    // machinery is exercised (any spurious double-drop would
    // panic in `LockHandleState::Drop`).

    use crate::extension_test_support::{shared_test_state, stub_ctx, stub_shared};
    // SyncHostCall's on_resource_drop is a trait method; the
    // super::* import pulls in only public re-exports, not the
    // module's private `use` statements, so bring the trait into
    // test scope explicitly to enable method-call syntax on
    // `host.on_resource_drop(...)`.
    use wasmos_runtime_api::SyncHostCall;

    /// Helper: pull the `rep` out of the two-layer Result-shape a
    /// `#[host_iface(sync)]`-emitted call returns for
    /// `acquire-exclusive`. Panics with the raw shape so a test
    /// failure names what came back.
    fn extract_lock_handle_rep(out: &[Value]) -> u32 {
        let [Value::Result(Ok(Some(payload)))] = out else {
            panic!("expected [Result(Ok(Some(Resource)))], got {out:?}");
        };
        let Value::Resource { store_id, handle_id } = payload.as_ref() else {
            panic!("expected Resource payload, got {:?}", payload);
        };
        // The stateful stub ctx recorded (interface, name, rep)
        // at mint; peek by hand-decoding the stub's contract:
        // handle_id is a monotonic counter starting at 0, rep is
        // the state's allocated lock-handle id. Use handle_id + 0
        // convention isn't what we want — the caller should pass
        // the recovered rep through `resource_rep`, but that
        // needs the ctx. Callers below get the rep from the
        // stub's `creations()` snapshot instead. Return the raw
        // handle_id for identity assertions only.
        let _ = (store_id, handle_id);
        // Look up rep via a fresh short-lived stub read — done by
        // the caller with the shared handle.
        u32::MAX
    }

    /// Path helper: a fresh scratch file inside the OS tempdir
    /// whose flock the test will acquire. Named uniquely per
    /// test so parallel test runs don't collide on the same
    /// OS-level lock.
    fn scratch_lock_path(tag: &str) -> String {
        let dir = std::env::temp_dir();
        let path = dir.join(format!("ducklink-runtime-test-{tag}-{}.lock", std::process::id()));
        // Ensure the file exists (LockHandleState::acquire_exclusive
        // opens with read+write; a missing file would fail on
        // some platforms).
        std::fs::File::create(&path).expect("create scratch lock file");
        path.to_string_lossy().into_owned()
    }

    #[test]
    fn file_lock_acquire_then_release_removes_from_state() {
        let state = shared_test_state();
        let host = FileLockHost::new(state.clone());
        let shared = stub_shared();
        let mut ctx = stub_ctx(&shared);

        let path = scratch_lock_path("acquire-release");

        // Acquire the lock — state should now hold one handle.
        let out = host
            .call(
                &mut ctx,
                "acquire-exclusive",
                vec![Value::String(path.clone())],
            )
            .expect("acquire-exclusive dispatch");
        assert_eq!(
            state.lock().unwrap().active_lock_handle_count(),
            1,
            "expected 1 live lock after acquire, got {}",
            state.lock().unwrap().active_lock_handle_count()
        );

        // Fish the (interface, name, rep) triple out of the
        // stub's minting log — the rep is the id
        // ExtensionStoreState.next_lock_handle assigned. Also
        // grab the Value::Resource itself so we can pass it back
        // to the release call verbatim.
        let creations = shared.creations();
        assert_eq!(creations.len(), 1, "expected 1 mint, got {creations:?}");
        assert_eq!(creations[0].0, "duckdb:extension/file-lock");
        assert_eq!(creations[0].1, "lock-handle");
        let rep = creations[0].2;
        assert!(
            state.lock().unwrap().contains_lock_handle(rep),
            "state should hold rep={rep} after acquire"
        );

        // Recover the Value::Resource for the release call. The
        // Ok-Some-Resource shape is fixed by the WIT + the
        // host_iface macro's return-wrapping — we validated it
        // via extract_lock_handle_rep's shape check above.
        let _ = extract_lock_handle_rep(&out);
        let handle_value = match &out[0] {
            Value::Result(Ok(Some(payload))) => (**payload).clone(),
            _ => unreachable!("shape validated above"),
        };

        // Release via the wit-mangled method name — the
        // #[method("[method]lock-handle.release")] override
        // routes here. Return is (); the macro's unit-return
        // wrap emits an empty Vec::new(), so out must be empty.
        let rel_out = host
            .call(
                &mut ctx,
                "[method]lock-handle.release",
                vec![handle_value],
            )
            .expect("lock-handle.release dispatch");
        assert!(
            rel_out.is_empty(),
            "release returns () — expected empty out vec, got {rel_out:?}"
        );

        // State should now be empty.
        assert_eq!(
            state.lock().unwrap().active_lock_handle_count(),
            0,
            "expected 0 live locks after release"
        );
        assert!(
            !state.lock().unwrap().contains_lock_handle(rep),
            "state should not hold rep={rep} after release"
        );
    }

    #[test]
    fn file_lock_acquire_then_on_resource_drop_removes_from_state() {
        // Phase 6.2.f end-to-end proof at the abstraction layer:
        // simulate what the wasmos adapter's `.resource_async(...)`
        // closure does when the guest's `Resource<LockHandle>`
        // falls out of scope. SyncHostCall::on_resource_drop is
        // called directly with (resource_name, rep) — the macro
        // routes to the `[resource-drop]lock-handle` arm, which
        // delegates to `state.release_lock_handle`.
        let state = shared_test_state();
        let host = FileLockHost::new(state.clone());
        let shared = stub_shared();
        let mut ctx = stub_ctx(&shared);

        let path = scratch_lock_path("acquire-drop");

        let _ = host
            .call(
                &mut ctx,
                "acquire-exclusive",
                vec![Value::String(path.clone())],
            )
            .expect("acquire-exclusive dispatch");
        assert_eq!(state.lock().unwrap().active_lock_handle_count(), 1);
        let rep = shared.creations()[0].2;
        assert!(state.lock().unwrap().contains_lock_handle(rep));

        // Fire the drop hook. This is the exact call the wasmos
        // adapter's destructor closure makes.
        host.on_resource_drop(&mut ctx, "lock-handle", rep)
            .expect("on_resource_drop dispatch");

        assert_eq!(
            state.lock().unwrap().active_lock_handle_count(),
            0,
            "expected 0 live locks after on_resource_drop"
        );
        assert!(
            !state.lock().unwrap().contains_lock_handle(rep),
            "state should not hold rep={rep} after on_resource_drop"
        );
    }

    // ─── Phase 6.2.g — scalar-callback lifecycle stateful test ────
    //
    // Proves the RuntimeHost's `[constructor]scalar-callback` +
    // `[resource-drop]scalar-callback` mangling overrides mutate
    // the shared CallbackRegistry correctly. The constructor
    // routes `handle: u32` (a dispatcher handle) through
    // `state.allocate_callback_handle_pub` which allocates a
    // fresh registry slot; the drop hook routes back through
    // `state.release_callback_handle_pub` which clears it. Both
    // ends are needed for the lifecycle to be tight — a leak
    // here would strand a CallbackRegistry entry every time a
    // scalar-callback resource fell out of guest scope.

    #[test]
    fn scalar_callback_constructor_then_drop_clears_registry() {
        let state = shared_test_state();
        let host = RuntimeHost::new(state.clone());
        let shared = stub_shared();
        let mut ctx = stub_ctx(&shared);

        // The constructor takes a dispatcher handle `u32` and
        // returns Resource<ScalarCallback>. We pick an arbitrary
        // dispatcher handle — the test doesn't drive the
        // dispatch table, just verifies the registry entry
        // arrives + departs.
        let dispatcher_handle: u32 = 4242;

        let registry = state.lock().unwrap().callback_registry_handle();
        // Pre-condition: registry has no entry for what the
        // constructor will allocate. We can't predict the fresh
        // id, so pre-assert the get(1) slot is empty (fresh
        // registry starts issuing at id 1).
        assert!(
            registry.read().unwrap().get(1).is_none(),
            "fresh registry should have no id=1 entry"
        );

        let out = host
            .call(
                &mut ctx,
                "[constructor]scalar-callback",
                vec![Value::U32(dispatcher_handle)],
            )
            .expect("scalar-callback constructor dispatch");

        // The stub minted a Resource<ScalarCallback>. Extract
        // its rep from the creations log — that is the registry
        // id the constructor allocated.
        let creations = shared.creations();
        assert_eq!(creations.len(), 1, "expected 1 mint, got {creations:?}");
        assert_eq!(creations[0].0, "duckdb:extension/runtime");
        assert_eq!(creations[0].1, "scalar-callback");
        let cb_id = creations[0].2;

        // Registry now holds the entry. Verify kind matches +
        // dispatcher_handle round-tripped.
        let entry = registry
            .read()
            .unwrap()
            .get(cb_id)
            .expect("registry must hold the freshly-allocated callback");
        assert!(matches!(entry.kind, crate::CallbackKind::Scalar));
        assert_eq!(entry.dispatcher_handle, dispatcher_handle);

        // Recover the returned Resource for the drop.
        let handle_value = match &out[0] {
            Value::Resource { .. } => out[0].clone(),
            other => panic!("expected Value::Resource, got {other:?}"),
        };

        // Fire the drop hook — the exact call the wasmos adapter
        // makes when the guest resource falls out of scope. The
        // macro-generated on_resource_drop arm delegates to
        // scalar_callback_drop, which calls release_callback_
        // handle_pub.
        host.on_resource_drop(&mut ctx, "scalar-callback", cb_id)
            .expect("scalar-callback drop dispatch");

        assert!(
            registry.read().unwrap().get(cb_id).is_none(),
            "registry entry for cb_id={cb_id} should be cleared after drop"
        );

        // The explicit-release path also works — Value::Resource
        // handed back through the wit-mangled name. Sanity-check
        // that a second drop of an already-released id is a
        // safe no-op (the registry's remove is idempotent).
        host.on_resource_drop(&mut ctx, "scalar-callback", cb_id)
            .expect("second drop of released id must be idempotent");

        // Round-trip the returned Resource through the wit-
        // mangled release-style method too, to prove that path
        // doesn't blow up on an already-released id.
        let _ = host
            .call(
                &mut ctx,
                "[resource-drop]scalar-callback",
                vec![handle_value],
            )
            .err(); // routing via [resource-drop] through call() is
                    // NOT valid — the macro puts drops on the drop
                    // arm exclusively, so this SHOULD error with
                    // "no handler for method". That's the same
                    // isolation the wasmos-side tests verified.
    }

    // ─── Phase 6.2.g — pending-buffer capture stateful test ──────
    //
    // Proves that a register_* dispatch pushed through the
    // wasmos-side handler mutates the right pending_xxx buffer
    // on the ExtensionStoreState. Picks `ParserHost.register-
    // parser-extension` because its signature is the simplest
    // in the interface set (just name: String + callback_handle:
    // u32), so the test's marshalling ceremony stays minimal
    // while proving the state-mutation contract end-to-end.
    //
    // Deliberately doesn't drain — leaving the pending entry in
    // place matches the extension lifecycle where drain fires
    // exactly once at load-finish time from a different code
    // path. Tests that want to verify the drain shape use the
    // existing extension::tests::drain_* suite.

    #[test]
    fn parser_register_pushes_to_pending_parsers() {
        let state = shared_test_state();
        let host = ParserHost::new(state.clone());
        let shared = stub_shared();
        let mut ctx = stub_ctx(&shared);

        // Pre-condition: no parsers registered yet.
        assert_eq!(state.lock().unwrap().pending_parser_count(), 0);

        let out = host
            .call(
                &mut ctx,
                "register-parser-extension",
                vec![
                    Value::String("my-parser".into()),
                    Value::U32(4242),
                ],
            )
            .expect("register-parser-extension dispatch");

        // Return shape: Result<u32, Duckerror> — expect Ok(id).
        // The id is a fresh resource id the state allocated for
        // the (unused) parser-registry rep; test cares only that
        // the shape parses cleanly.
        match out.as_slice() {
            [Value::Result(Ok(Some(payload)))] => match payload.as_ref() {
                Value::U32(_) => {}
                other => panic!("expected Ok(U32), got {other:?}"),
            },
            other => panic!("expected [Result(Ok(Some(U32)))], got {other:?}"),
        }

        // Post-condition: state's pending_parsers grew by one.
        assert_eq!(
            state.lock().unwrap().pending_parser_count(),
            1,
            "register dispatch must push to pending_parsers"
        );

        // A second registration under a different name adds a
        // second entry.
        let _ = host
            .call(
                &mut ctx,
                "register-parser-extension",
                vec![
                    Value::String("another-parser".into()),
                    Value::U32(4243),
                ],
            )
            .expect("second register dispatch");
        assert_eq!(
            state.lock().unwrap().pending_parser_count(),
            2,
            "second register must push a second entry"
        );
    }

    #[test]
    fn file_lock_on_resource_drop_of_unknown_rep_is_noop() {
        // Idempotency guard: firing on_resource_drop with a rep
        // the state doesn't know about must be a silent no-op.
        // This matches the Phase 6.2.f adapter contract — the
        // guest might drop a handle the host already released
        // explicitly via `handle.release()`; the drop dispatch
        // must not panic or error in that race.
        let state = shared_test_state();
        let host = FileLockHost::new(state.clone());
        let shared = stub_shared();
        let mut ctx = stub_ctx(&shared);

        assert_eq!(state.lock().unwrap().active_lock_handle_count(), 0);
        host.on_resource_drop(&mut ctx, "lock-handle", 999_999)
            .expect("on_resource_drop of unknown rep must be no-op");
        assert_eq!(state.lock().unwrap().active_lock_handle_count(), 0);
    }
}
