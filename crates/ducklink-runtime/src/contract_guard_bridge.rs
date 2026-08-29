//! Bridge from wasmtime types to `wasmos-runtime-api::CompiledComponent`
//! for the contract-version load guard — ADR-0029 Phase 6.1b (ducklink
//! side, follows the wasmos-side landing at wasmos commit 8579294 +
//! datalink migration at datalink commit eadf4d4).
//!
//! ## Why this file exists
//!
//! `datalink-contract` migrated its two introspection fns from
//! `(engine: &Engine, component: &Component, package: &str)` to
//! `(component: &CompiledComponent, package: &str)`. This crate's
//! wrapper fns (`component_contract_major`, `component_contract_version`,
//! `check_component_contract`) keep their existing signatures — 500+
//! internal call sites across ducklink production + tests + downstream
//! consumers pass raw wasmtime types, and migrating every one of them
//! is a separate later phase. This bridge lets the wrappers translate
//! at the call boundary without any consumer-visible API break.
//!
//! ## Design shape
//!
//! `WasmtimeComponentAdapter` implements
//! `wasmos_runtime_api::CompiledComponentImpl` over a borrowed
//! `wasmtime::Engine + Component + name`. The impl body of
//! `imported_instance_names` is a one-line
//! `component.component_type().imports(&engine).map(...).collect()` —
//! identical to what `datalink-contract`'s pre-migration inline code
//! did before it went through the abstraction. Net cost: an
//! `Arc<dyn CompiledComponentImpl>` allocation per guard call
//! (single-shot per component load — not a hot path).
//!
//! ## Retirement path
//!
//! This module goes away when ducklink-runtime's public wrappers
//! themselves migrate to `(&CompiledComponent, ...)` sigs — i.e. when
//! the whole compile-side of ducklink runs through
//! `wasmos-runtime-api::Runtime::compile_component` and every consumer
//! carries `CompiledComponent`s instead of `wasmtime::component::Component`s.
//! Until then this stays as the bridge.

use std::any::Any;
use std::sync::Arc;

use wasmos_runtime_api::component::{CompiledComponent, CompiledComponentImpl};
use wasmtime::component::Component;
use wasmtime::Engine;

/// A `CompiledComponentImpl` that wraps an already-compiled wasmtime
/// `Engine + Component` pair. **Not for consumer use** — this exists
/// solely to bridge ducklink-runtime's legacy wrappers to the
/// abstraction-shaped `datalink-contract` API.
struct WasmtimeComponentAdapter {
    engine: Engine,
    component: Component,
    name: String,
}

impl CompiledComponentImpl for WasmtimeComponentAdapter {
    fn name(&self) -> &str {
        &self.name
    }

    fn as_any(&self) -> &dyn Any {
        self
    }

    fn imported_instance_names(&self) -> Vec<String> {
        // Same iteration + string conversion `datalink-contract` used
        // to do inline before Phase 6.1b — enumerate the component's
        // imports and canonicalise each instance name.
        self.component
            .component_type()
            .imports(&self.engine)
            .map(|(name, _)| name.to_string())
            .collect()
    }
}

/// Wrap a (borrowed) wasmtime `Engine + Component` pair into a
/// `wasmos_runtime_api::CompiledComponent` for the duration of one
/// call. Clones the engine (Arc bump) and the component (also
/// Arc-shaped internally); the returned handle owns the clones so it
/// outlives the borrow.
///
/// The `name` argument is diagnostic-only. ducklink's wrappers pass an
/// empty string when the extension name isn't handy at the call site;
/// this is fine — the guard doesn't consult the name.
pub(crate) fn wrap_wasmtime_component(
    engine: &Engine,
    component: &Component,
    name: &str,
) -> CompiledComponent {
    CompiledComponent::from_impl(Arc::new(WasmtimeComponentAdapter {
        engine: engine.clone(),
        component: component.clone(),
        name: name.to_string(),
    }))
}
