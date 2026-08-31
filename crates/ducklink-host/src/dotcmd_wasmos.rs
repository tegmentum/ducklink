//! ADR-0029 Phase 6.2.d.3 — wasmos-native mirror of the
//! `duckdb:dotcmd/spi` host interface.
//!
//! This is the counterpart to `ducklink_runtime::extension_wasmos`
//! for the dot-command guest world. Where `extension_wasmos` mirrors
//! the 27 extension-side interfaces of `ExtensionStoreState`, this
//! module mirrors the single 2-method `spi` interface that
//! `DotcmdState` implements at line 1337 of `crate::lib`.
//!
//! # What's here
//!
//! * `SpiHost` — the `#[host_iface(sync)]`-decorated host struct
//!   covering the two `duckdb:dotcmd/spi` methods:
//!   - `query(sql) -> result<string, string>` — runs SQL against
//!     the CLI's active connection through
//!     `CoreExecution::with_database`.
//!   - `edit(initial, hint-suffix) -> result<string, string>` —
//!     shells out to `$EDITOR` for multi-line input.
//! * `install_spi_imports(imports, core, current_connection) ->
//!   HostImports` — one-line registration on a `HostImports` set.
//!
//! # Design notes
//!
//! `DotcmdState`'s fields are already `Arc<Mutex<...>>` shared
//! handles (`core: Arc<Mutex<CoreExecution>>` +
//! `current_connection: Arc<Mutex<Option<ResourceAny>>>`), so the
//! wasmos-native handler captures them individually — no wrap-the-
//! whole-state pattern needed. This is cleaner than the
//! `SharedExtensionState = Arc<Mutex<ExtensionStoreState>>` pattern
//! from Phase 6.2.d.2, and enabled by DotcmdState's smaller
//! surface.
//!
//! Sync throughout via `#[host_iface(sync)]` — the existing
//! wit-bindgen impl (`crate::lib`:1337) is sync; no async wrapping.
//!
//! # Wasmtime-tie-in
//!
//! `CoreExecution` internally holds a `wasmtime::Store` and
//! `ResourceAny` (via `current_connection`) is a wasmtime type.
//! These types don't cross the wasmos abstraction's public surface
//! — the handler methods return only `String` and error `String`.
//! The wasmtime types live entirely inside the shared-handle
//! closures, treated as opaque by the wasmos install path.
//!
//! # Coexistence
//!
//! Additive; the existing wit-bindgen `impl dotcmd_bindings::
//! duckdb::dotcmd::spi::Host for DotcmdState` at `crate::lib`:1337
//! stays. Consumers pick per instantiation.

use std::sync::{Arc, Mutex};

use wasmos_runtime_api::{
    host_iface, HostCallContext, HostImports, RuntimeResult, SyncHostCall, SyncHostCallAdapter,
};
use wasmtime::component::ResourceAny;

use crate::{core_duckerror_message, spi_edit, spi_render_rows, CoreExecution};

/// Host struct for the `duckdb:dotcmd/spi` interface.
///
/// Captures both shared handles from `DotcmdState`:
/// - `core`: the CLI's `CoreExecution` (wasmtime store + core
///   bindings) shared between the primary command loop and any
///   dot-command that opts in to `spi.query`.
/// - `current_connection`: the CLI's active database connection
///   handle, populated by the shell's `.open` / `.close` commands.
#[derive(Clone)]
pub struct SpiHost {
    core: Arc<Mutex<CoreExecution>>,
    current_connection: Arc<Mutex<Option<ResourceAny>>>,
}

impl SpiHost {
    /// Construct a new `SpiHost` capturing the two shared handles.
    pub fn new(
        core: Arc<Mutex<CoreExecution>>,
        current_connection: Arc<Mutex<Option<ResourceAny>>>,
    ) -> Self {
        Self {
            core,
            current_connection,
        }
    }
}

#[host_iface(sync)]
impl SpiHost {
    /// Handler for `duckdb:dotcmd/spi.query`. Byte-identical
    /// semantics to `crate::lib::DotcmdState::query` at line 1338:
    /// no active connection → Err("spi: no active database
    /// connection"); executor trap → Err("spi query trapped: {trap}");
    /// core `Duckerror` → Err(rendered-message); success →
    /// Ok(rendered-rows).
    fn query(&self, _ctx: &mut HostCallContext<'_>, sql: String) -> RuntimeResult<Result<String, String>> {
        let handle = self
            .current_connection
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone();
        let handle = match handle {
            Some(h) => h,
            None => return Ok(Err("spi: no active database connection".to_string())),
        };
        let mut core = self.core.lock().unwrap_or_else(|e| e.into_inner());
        let result = match core
            .with_database(|guest, store| guest.call_execute(store, handle, &sql))
        {
            Ok(r) => r,
            Err(trap) => return Ok(Err(format!("spi query trapped: {trap}"))),
        };
        Ok(match result {
            Ok(qr) => Ok(spi_render_rows(qr)),
            Err(err) => Err(core_duckerror_message(err)),
        })
    }

    /// Handler for `duckdb:dotcmd/spi.edit`. Delegates to the free
    /// function `crate::lib::spi_edit` — no state dependency, so
    /// the trait method is effectively a passthrough.
    fn edit(
        &self,
        _ctx: &mut HostCallContext<'_>,
        initial: String,
        hint_suffix: String,
    ) -> RuntimeResult<Result<String, String>> {
        Ok(spi_edit(&initial, &hint_suffix))
    }
}

/// Register the `duckdb:dotcmd/spi` handler on the given
/// [`HostImports`] set. Consumer usage:
///
/// ```rust,ignore
/// let imports = ducklink_host::dotcmd_wasmos::install_spi_imports(
///     wasmos_runtime_api::HostImports::new(),
///     core.clone(),
///     current_connection.clone(),
/// );
/// // Thread `imports` into the ExecutionContext at instantiate time.
/// ```
///
/// Interface name matches the WIT surface exactly:
/// `duckdb:dotcmd/spi`. Wasmos does verbatim interface-name
/// matching; the dot-command world is unversioned today.
pub fn install_spi_imports(
    imports: HostImports,
    core: Arc<Mutex<CoreExecution>>,
    current_connection: Arc<Mutex<Option<ResourceAny>>>,
) -> HostImports {
    let host = SpiHost::new(core, current_connection);
    imports.register(
        "duckdb:dotcmd/spi",
        Arc::new(SyncHostCallAdapter::new(host)) as Arc<dyn wasmos_runtime_api::HostCall>,
    )
}

// Silence unused-import warning: SyncHostCall is only reached
// through the `#[host_iface(sync)]` macro's emitted impl block.
// The import stays so grep-locality holds.
#[allow(dead_code)]
const _SYNC_HOST_CALL: fn() = || {
    fn _assert<T: SyncHostCall>() {}
    _assert::<SpiHost>();
};

// Behavior tests for SpiHost need a real `CoreExecution` fixture
// (wasmtime Store + duckdb bindings + open temp DB). Deferred to
// Phase 6.2.d.3-b when the fixture lands — the surface here
// compile-verifies via `_SYNC_HOST_CALL` above.
