//! Shared test-support fixture for `extension` + `extension_wasmos` unit
//! tests (ADR-0029 Phase 6.2.g).
//!
//! Extracted from the two crates' individual `#[cfg(test)] mod tests` blocks
//! so both the wit-bindgen-shaped path (`extension::tests`) and the
//! wasmos-native path (`extension_wasmos::tests`) share one canonical
//! `ExtensionStoreState` builder + one canonical `NoopServices` sink.
//!
//! ## What lives here
//!
//! - [`NoopServices`] — a `Send` [`ExtensionServices`] impl that reports
//!   every config read as unavailable and drops every log line. Enough to
//!   satisfy `ExtensionStoreState::new`; not enough for tests that need
//!   to assert on services calls (those inject their own sink).
//! - [`test_state`] — a fresh `ExtensionStoreState` wired to `NoopServices`
//!   + a fresh, empty `CallbackRegistry`. Consumed by the many state-
//!   mutation unit tests inside `extension::tests`.
//! - [`shared_test_state`] — the same state wrapped in the
//!   [`SharedExtensionState`] shape that `extension_wasmos`'s
//!   `#[host_iface(sync)]` handlers hold. This is the Phase 6.2.d.2 /
//!   6.2.f entry point for stateful integration tests on the wasmos-
//!   native mirror.
//! - [`StatefulStubCtx`] — a stub [`HostCallCtxImpl`] that maintains an
//!   internal (store_id, handle_id) → rep table so `Value::Resource`
//!   marshalling round-trips cleanly at the abstraction layer, without
//!   needing a real wasmtime `Store` for construction. Mirrors the
//!   `StubCtxState` pattern used inside wasmos's own
//!   `host_iface_resource.rs` test file.
//! - [`stub_shared`] / [`stub_ctx`] — the two-step ctor split that
//!   lets a test hold onto the shared inspection handle while the ctx
//!   is passed by mutable reference to the SyncHostCall dispatch.
//!
//! ## Not in scope
//!
//! Guest-driven end-to-end tests (real component instantiation with
//! wasmtime, drop-fire via the adapter closure) still need a purpose-
//! built .wasm fixture; those live in `tests/*.rs` per the existing
//! integration-test convention. The support here is for hand-crafted
//! `SyncHostCall::call` / `on_resource_drop` invocations against the
//! real `ExtensionStoreState`.

#![cfg(test)]

use std::collections::HashMap;
use std::sync::{Arc, Mutex, RwLock};

use wasmos_runtime_api::{HostCallContext, HostCallCtxImpl, RuntimeError, RuntimeResult, Value};

use crate::extension::{ConfigError, ExtensionServices, ExtensionStoreState, LogField, LogLevel};
use crate::extension_wasmos::SharedExtensionState;
use crate::CallbackRegistry;

// ─── ExtensionServices stub ────────────────────────────────────────

/// A no-op `ExtensionServices` sink: every config read reports
/// unavailable, every log line is dropped. Lets a test build an
/// `ExtensionStoreState` for the capture / lifecycle paths without a
/// live database or a real config source. Tests that need to assert
/// on services calls (query, log messages, nested-exec traces) inject
/// their own recording sink instead.
pub(crate) struct NoopServices;

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

// ─── ExtensionStoreState builders ──────────────────────────────────

/// Build a fresh, sandboxed `ExtensionStoreState` wired to
/// `NoopServices` + an empty `CallbackRegistry`. Extension name is
/// `"testext"`; that's the label the state uses in Duckerror messages
/// so tests asserting on error text should expect it.
pub(crate) fn test_state() -> ExtensionStoreState {
    let wasi = wasmtime_wasi::WasiCtxBuilder::new().build();
    ExtensionStoreState::new(
        wasi,
        Box::new(NoopServices),
        Arc::new(RwLock::new(CallbackRegistry::default())),
        "testext".to_string(),
    )
}

/// The wasmos-native mirror's `SharedExtensionState` wrapping a
/// fresh [`test_state`]. Consumed by `extension_wasmos`'s
/// `#[host_iface(sync)]` handlers, which each accept
/// `Arc<Mutex<ExtensionStoreState>>` at construction time.
pub(crate) fn shared_test_state() -> SharedExtensionState {
    Arc::new(Mutex::new(test_state()))
}

// ─── Stateful stub context ─────────────────────────────────────────

/// The mutable inner-state of [`StatefulStubCtx`]. Kept in an
/// `Arc<Mutex<_>>` so a test can retain an inspection handle
/// (via [`stub_shared`]) alongside the `&mut` the ctx yields to
/// dispatch.
///
/// Records every `new_host_resource` call (interface + resource
/// name + rep) so tests can assert on the mint side, and maintains
/// a `(store_id, handle_id) → rep` table so `resource_rep` can
/// recover the rep the mint recorded — matching the shape wasmos's
/// v48 adapter maintains via `AdapterHostState.resources`.
#[derive(Default)]
pub(crate) struct StatefulStubCtxState {
    /// Every `(interface, resource_name, rep)` tuple minted so far.
    /// Tests use this to assert on which resources got allocated
    /// during a dispatch (e.g. a constructor should mint exactly
    /// one; a drop should mint none).
    pub creations: Vec<(String, String, u32)>,
    /// (store_id, handle_id) → rep — for `resource_rep` lookups
    /// so a `Value::Resource` handed to a subsequent dispatch call
    /// can recover the rep the first dispatch minted.
    pub reps: HashMap<(u64, u64), u32>,
    /// Next handle_id the stub will hand out. Monotonic; never
    /// reused (matches the v48 adapter's per-store counter).
    pub next_handle: u64,
}

/// Shared handle to a [`StatefulStubCtxState`]. Cheap to `Clone`; a
/// test typically keeps one clone for inspection and passes another
/// into [`stub_ctx`] where it becomes the ctx impl's state.
#[derive(Clone, Default)]
pub(crate) struct StatefulStubShared {
    inner: Arc<Mutex<StatefulStubCtxState>>,
}

impl StatefulStubShared {
    /// Snapshot every `(iface, name, rep)` the stub has minted so
    /// far. Cloning `String`s per call is fine for test assertions
    /// and lets the caller drop the shared handle immediately.
    pub fn creations(&self) -> Vec<(String, String, u32)> {
        self.inner.lock().unwrap().creations.clone()
    }
    /// Count of `new_host_resource` calls to date. Convenience for
    /// tests that only care about how many resources were minted,
    /// not their identity.
    #[allow(dead_code)]
    pub fn creation_count(&self) -> usize {
        self.inner.lock().unwrap().creations.len()
    }
}

/// A stub [`HostCallCtxImpl`] that lets a test call
/// `SyncHostCall::call` / `on_resource_drop` without wiring a real
/// wasmtime `Store`. Round-trips `Value::Resource` through an
/// internal `(store_id, handle_id) → rep` map so a subsequent
/// dispatch can recover the rep the first dispatch minted.
///
/// Constructed via [`stub_shared`] + [`stub_ctx`]; the split lets
/// the test hold onto the inspection handle while passing the ctx
/// by `&mut` into the dispatch call.
pub(crate) struct StatefulStubCtx {
    shared: StatefulStubShared,
}

impl HostCallCtxImpl for StatefulStubCtx {
    fn new_host_resource(
        &mut self,
        interface: &str,
        resource_name: &str,
        rep: u32,
    ) -> RuntimeResult<Value> {
        let mut s = self.shared.inner.lock().unwrap();
        s.creations
            .push((interface.to_string(), resource_name.to_string(), rep));
        let handle_id = s.next_handle;
        s.next_handle += 1;
        // Store id is a fixed sentinel — the stub doesn't run
        // multiple stores so cross-store rejection isn't relevant.
        // Keeping the value stable across mints matches the v48
        // adapter's per-store id.
        let store_id = 0xCAFEu64;
        s.reps.insert((store_id, handle_id), rep);
        Ok(Value::Resource { store_id, handle_id })
    }

    fn resource_rep(&mut self, value: &Value) -> RuntimeResult<u32> {
        let Value::Resource { store_id, handle_id } = value else {
            return Err(RuntimeError::msg(format!(
                "test-support resource_rep: expected Value::Resource, got {value:?}"
            )));
        };
        let s = self.shared.inner.lock().unwrap();
        s.reps.get(&(*store_id, *handle_id)).copied().ok_or_else(|| {
            RuntimeError::msg(format!(
                "test-support resource_rep: no rep recorded for \
                 (store_id={store_id}, handle_id={handle_id})"
            ))
        })
    }
}

/// Build a fresh, empty [`StatefulStubShared`] a test can retain for
/// inspection.
pub(crate) fn stub_shared() -> StatefulStubShared {
    StatefulStubShared::default()
}

/// Build a `HostCallContext<'static>` around a fresh [`StatefulStubCtx`]
/// backed by `shared`. The context is `'static` because the underlying
/// stub is boxed and leaked — tests are short-lived and this sidesteps
/// the lifetime dance of threading a `&mut` through the dispatch call
/// site.
///
/// This is the equivalent of the `ctx_of` helper inside wasmos's own
/// `host_iface_resource.rs` test file; ducklink's tests reach for the
/// same shape.
pub(crate) fn stub_ctx(shared: &StatefulStubShared) -> HostCallContext<'static> {
    let boxed: &'static mut StatefulStubCtx = Box::leak(Box::new(StatefulStubCtx {
        shared: shared.clone(),
    }));
    HostCallContext::new(boxed)
}
