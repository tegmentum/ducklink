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
    host_iface, HostCallContext, HostImports, RuntimeResult, SyncHostCall, SyncHostCallAdapter,
    WitEnum, WitFlags, WitVariant,
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
    install_query_imports(imports, state)
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
    state: SharedExtensionState,
}

impl ParserHost {
    pub fn new(state: SharedExtensionState) -> Self {
        Self { state }
    }
}

#[host_iface(sync)]
impl ParserHost {
    fn register_parser_extension(
        &self,
        _ctx: &mut HostCallContext<'_>,
        name: String,
        callback_handle: u32,
    ) -> RuntimeResult<Result<u32, Duckerror>> {
        let mut g = self.state.lock().expect("ExtensionStoreState mutex poisoned");
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
    state: SharedExtensionState,
}

impl OptimizerHost {
    pub fn new(state: SharedExtensionState) -> Self {
        Self { state }
    }
}

#[host_iface(sync)]
impl OptimizerHost {
    fn register_optimizer_rule(
        &self,
        _ctx: &mut HostCallContext<'_>,
        rule_name: String,
        callback_handle: u32,
    ) -> RuntimeResult<Result<u32, Duckerror>> {
        let mut g = self.state.lock().expect("ExtensionStoreState mutex poisoned");
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
    state: SharedExtensionState,
}

impl SettingsHost {
    pub fn new(state: SharedExtensionState) -> Self {
        Self { state }
    }
}

#[host_iface(sync)]
impl SettingsHost {
    fn register_option(
        &self,
        _ctx: &mut HostCallContext<'_>,
        name: String,
        description: String,
        ty: SettingType,
        default_value: Option<String>,
        scope: SettingScope,
    ) -> RuntimeResult<Result<(), Duckerror>> {
        let ty_str = ty.as_str().to_string();
        let scope_str = scope.as_str().to_string();
        let mut g = self.state.lock().expect("ExtensionStoreState mutex poisoned");
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
    state: SharedExtensionState,
}

impl CoordinateSystemHost {
    pub fn new(state: SharedExtensionState) -> Self {
        Self { state }
    }
}

#[host_iface(sync)]
impl CoordinateSystemHost {
    fn register_coordinate_system(
        &self,
        _ctx: &mut HostCallContext<'_>,
        crs: CrsDef,
    ) -> RuntimeResult<Result<u32, Duckerror>> {
        let mut g = self.state.lock().expect("ExtensionStoreState mutex poisoned");
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
    state: SharedExtensionState,
}

impl StorageHost {
    pub fn new(state: SharedExtensionState) -> Self {
        Self { state }
    }
}

#[host_iface(sync)]
impl StorageHost {
    fn register_storage(
        &self,
        _ctx: &mut HostCallContext<'_>,
        type_name: String,
        callback_handle: u32,
        options: Option<Extopts>,
    ) -> RuntimeResult<Result<u32, Duckerror>> {
        let neutral_options = options.map(|o| crate::reg::ExtOpts {
            description: o.description,
            tags: o.tags,
        });
        let mut g = self.state.lock().expect("ExtensionStoreState mutex poisoned");
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
    state: SharedExtensionState,
}

impl LogStorageHost {
    pub fn new(state: SharedExtensionState) -> Self {
        Self { state }
    }
}

#[host_iface(sync)]
impl LogStorageHost {
    fn register_log_storage(
        &self,
        _ctx: &mut HostCallContext<'_>,
        name: String,
        callback_handle: u32,
    ) -> RuntimeResult<Result<u32, Duckerror>> {
        let mut g = self.state.lock().expect("ExtensionStoreState mutex poisoned");
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
    state: SharedExtensionState,
}

impl QueryHost {
    pub fn new(state: SharedExtensionState) -> Self {
        Self { state }
    }
}

#[host_iface(sync)]
impl QueryHost {
    fn query(
        &self,
        _ctx: &mut HostCallContext<'_>,
        sql: String,
    ) -> RuntimeResult<Result<Vec<Vec<String>>, String>> {
        let mut g = self.state.lock().expect("ExtensionStoreState mutex poisoned");
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
}
