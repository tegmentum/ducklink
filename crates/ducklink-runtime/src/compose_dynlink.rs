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

use wasmtime::component::ResourceTable;
use wasmtime_wasi::{WasiCtx, WasiCtxView, WasiView};

// The shared machinery. ducklink's public surface (ProviderRegistry,
// ProviderPreopen, imports_linker, add_to_linker, the bindings module) is
// re-exported verbatim so callers (extension.rs, ducklink-host, the dlopen
// test) are unchanged.
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

/// Host-side store state for driving a `compose:dynlink/linker` guest.
/// Carries WASI (the guest is typically a `wasi:cli/run` component) plus the
/// dynlink bridge. Used by the standalone dlopen test.
pub struct DynState {
    wasi: WasiCtx,
    wasi_table: ResourceTable,
    bridge: DynLinkBridge,
}

impl DynState {
    pub fn new(wasi: WasiCtx, wasi_table: ResourceTable, registry: ProviderRegistry) -> Self {
        Self {
            wasi,
            wasi_table,
            bridge: new_resident(registry),
        }
    }

    /// Expose the bridge for the trait-impl macro.
    pub fn dynlink_bridge(&mut self) -> &mut DynLinkBridge {
        &mut self.bridge
    }
}

impl WasiView for DynState {
    fn ctx(&mut self) -> WasiCtxView<'_> {
        WasiCtxView {
            ctx: &mut self.wasi,
            table: &mut self.wasi_table,
        }
    }
}

impl_compose_dynlink_host!(DynState, dynlink_bridge);
