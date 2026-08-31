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

use std::sync::Arc;

use wasmos_runtime_api::{
    host_iface, HostCallContext, HostImports, RuntimeResult, SyncHostCall, SyncHostCallAdapter,
    WitFlags, WitVariant,
};

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
