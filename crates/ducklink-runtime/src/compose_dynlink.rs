//! ducklink's `compose:dynlink/linker` host — a THIN adapter over the shared
//! [`datalink_dynlink`] crate.
//!
//! The resolve/invoke resource-table machinery, the generated linker
//! bindings, and the resident-provider lifecycle (instantiate-ONCE-and-reuse,
//! with preopened dirs — the warmed pylon serving many aggregate components)
//! all live in `datalink_dynlink` now, shared with the other wasm-component
//! hosts. This module only:
//!   - re-exports the registry + preopen + linker plumbing,
//!   - exposes ducklink's `DynLinkBridge` as the shared bridge specialized to
//!     the resident backend (+ a `new_resident` convenience),
//!   - keeps ducklink's standalone `DynState` (used by the dlopen test), and
//!   - re-exports `impl_compose_dynlink_host!` so the dot-command + extension
//!     store types impl the Host traits exactly as before.
//!
//! ## Shared / resident model (unchanged)
//!
//! A registered provider is instantiated ONCE (lazily, on first resolve) into
//! a single resident store, and every subsequent `resolve_by_id` for the same
//! id hands back a handle pointing at that ONE resident instance. All
//! `invoke`s drive the same provider store — the "one heavy provider serving
//! many function components" property — implemented by
//! [`datalink_dynlink::ResidentBackend`].

// The shared machinery. ducklink's public surface (ProviderRegistry,
// ProviderPreopen, imports_linker, add_to_linker, the bindings module) is
// re-exported verbatim so callers (extension.rs, ducklink-host, the dlopen
// test) are unchanged.
//
// ADR-0029 Phase 6.2.d.1: DynState + `impl WasiView for DynState` +
// `impl_compose_dynlink_host!(DynState, ...)` moved OUT (see the block
// below the pub-use). WasiCtx / ResourceTable / WasiView imports are no
// longer needed at module scope; ExtensionStoreState (extension.rs) +
// DotcmdState (ducklink-host) still expand the macro but import wasmtime
// types via their own module-scope use statements.
pub use datalink_dynlink::{
    add_to_linker, bindings, imports_linker, ProviderPreopen, ProviderRegistry, ResidentBackend,
};

/// ducklink's dynlink bridge: the shared store-generic bridge specialized to
/// the resident-provider backend. Construct it with [`new_resident`] (or the
/// inherent shared `DynLinkBridge::new(ResidentBackend::new(registry))`).
pub type DynLinkBridge = datalink_dynlink::DynLinkBridge<ResidentBackend>;

/// Build the resident-backed dynlink bridge over a shared provider registry.
/// Convenience preserving the pre-consolidation `DynLinkBridge::new(registry)`
/// ergonomics at the call sites.
pub fn new_resident(registry: ProviderRegistry) -> DynLinkBridge {
    DynLinkBridge::new(ResidentBackend::new(registry))
}

/// Implement the `compose:dynlink/linker` Host + HostInstance traits for a
/// store type that exposes a `&mut DynLinkBridge` via the named accessor.
/// Thin wrapper over [`datalink_dynlink::impl_datalink_dynlink_host!`] that
/// fixes the backend to [`ResidentBackend`] so the two-argument call form used
/// across ducklink (`impl_compose_dynlink_host!(Ty, accessor)`) is preserved.
#[macro_export]
macro_rules! impl_compose_dynlink_host {
    ($ty:ty, $bridge:ident) => {
        $crate::datalink_dynlink::impl_datalink_dynlink_host!(
            $ty,
            $crate::compose_dynlink::ResidentBackend,
            $bridge
        );
    };
}

// ADR-0029 Phase 6.2.d.1: DynState migrated OUT of this module.
//
// DynState was a wasmtime-Store state type used only by the standalone
// dlopen integration test (crates/ducklink-host/tests/compose_dynlink_dlopen.rs).
// The test has been rewritten to consume datalink-dynlink-wasmos v0.1.0
// end-to-end through the wasmos-runtime-api abstraction — no wasmtime
// Store/Linker/WasiCtx exposed to the test code — so the intermediate
// DynState wrapper is no longer needed.
//
// The impl_compose_dynlink_host!(DynState, dynlink_bridge) macro
// expansion was also removed; the abstraction-based test constructs
// its bridge as an Arc<dyn HostCall> via
// datalink_dynlink_wasmos::install_host_imports.
//
// ExtensionStoreState + DotcmdState (production paths) continue to
// use impl_compose_dynlink_host! and consume this module's
// wasmtime-shaped surface. They migrate in Phase 6.2.d.2 / 6.2.d.3.
