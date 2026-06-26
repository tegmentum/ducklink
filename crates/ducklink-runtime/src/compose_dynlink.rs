//! Native (wasmtime) host implementation of `compose:dynlink/linker` for
//! ducklink-host — "dlopen for components", the foundation for one heavy
//! SHARED provider (e.g. pylon) serving many lightweight function
//! components.
//!
//! A guest imports `compose:dynlink/linker`, resolves a provider by id
//! (or digest), and `invoke(method, payload)`s it through an opaque,
//! host-owned handle. The host forwards the bytes verbatim to the
//! provider's `compose:dynlink/endpoint.handle` export — no typed WIT
//! values cross the boundary, so the nominal-resource type-identity
//! problem never arises.
//!
//! This mirrors the framework's `hosts/wasmtime/src/dynlink.rs` reference
//! and sqlink's `host/src/compose_provider.rs`, but implements the
//! `linker` Host trait over ducklink's own store state rather than the
//! framework's `DynState` (whose `Linker<DynState>` is hardcoded to its
//! store type).
//!
//! ## Shared vs per-invoke
//!
//! The framework reference instantiates a FRESH provider store per
//! `resolve`. ducklink's goal is the **shared / resident** model: a
//! registered provider is instantiated ONCE (lazily, on first resolve)
//! into a single resident [`Store`], and every subsequent `resolve_by_id`
//! for the same id hands back a handle pointing at that ONE resident
//! instance. All `invoke`s — across every guest, across every resolve —
//! drive the same provider store. That is exactly the "one heavy provider
//! serving many function components" property: the provider's heap is
//! warmed once and shared (a pylon-shaped provider amortizes its load).

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use wasmtime::component::{Component, Linker, Resource, ResourceTable};
use wasmtime::{Engine, Store};
use wasmtime_wasi::{
    DirPerms, FilePerms, ResourceTable as WasiResourceTable, WasiCtx, WasiCtxBuilder, WasiCtxView,
    WasiView,
};

/// A directory to preopen into a provider's OWN store, mounted at `guest`
/// (e.g. `/lib`) from the host path `host`. A pylon-shaped provider needs
/// its CPython `Lib` (with bundled numpy) and its dispatcher `pylib` dir
/// preopened so the resident interpreter can import them.
#[derive(Clone, Debug)]
pub struct ProviderPreopen {
    /// Host filesystem path to expose.
    pub host: PathBuf,
    /// Guest mount point (e.g. "/lib" or "/app").
    pub guest: String,
}

impl ProviderPreopen {
    pub fn new(host: impl Into<PathBuf>, guest: impl Into<String>) -> Self {
        Self {
            host: host.into(),
            guest: guest.into(),
        }
    }
}

/// Bindgen for the guest-facing `compose:dynlink/linker` import. We only
/// need the host (import) side: the `linker` interface + the `instance`
/// resource. The `instance` resource is mapped to our backing
/// [`DynInstance`] type.
pub mod bindings {
    wasmtime::component::bindgen!({
        path: "../../wit/compose-dynlink",
        world: "dynlink-guest",
    });
}

/// Bindgen for instantiating a *provider* component — one that exports
/// `compose:dynlink/endpoint`. Kept in its own module so its generated
/// `compose::dynlink` / `sys::compose` types don't collide with the
/// guest-side `linker` bindings above.
pub mod provider {
    wasmtime::component::bindgen!({
        path: "../../wit/compose-dynlink",
        world: "dynlink-provider",
    });
}

use bindings::compose::dynlink::linker::Instance;
use bindings::sys::compose::types::{Error, ErrorCode};

/// Retype a guest-facing `Resource<Instance>` to the host backing type
/// `Resource<DynInstance>`. The two share the same table rep — the type
/// parameter is only a host-side compile-time tag — so this is a sound
/// reinterpretation (mirrors the framework's `as_backing`).
fn as_backing(r: &Resource<Instance>) -> Resource<DynInstance> {
    Resource::new_own(r.rep())
}

/// Minimal WASI store-state for a resident provider's OWN store. Provider
/// components pull in WASI even for trivial logic (std), so the store
/// carries a minimal WASI context.
struct ProviderState {
    wasi: WasiCtx,
    table: WasiResourceTable,
}

impl WasiView for ProviderState {
    fn ctx(&mut self) -> WasiCtxView<'_> {
        WasiCtxView {
            ctx: &mut self.wasi,
            table: &mut self.table,
        }
    }
}

/// A resident, instantiated provider. Instantiated ONCE (lazily) and
/// shared across every resolve/invoke. Holds its own store so calls into
/// the provider never touch the calling guest's store.
struct ResidentProvider {
    store: Store<ProviderState>,
    instance: provider::DynlinkProvider,
}

/// Registration record for a provider id. The component is compiled at
/// registration time; the resident instance is materialized lazily on the
/// first resolve (and then reused).
struct ProviderEntry {
    component: Component,
    /// `Some(..)` once instantiated; reused across all subsequent
    /// resolves and invokes (the shared model).
    resident: Option<ResidentProvider>,
    path: PathBuf,
    /// Directories to preopen into the provider's OWN store when it is
    /// materialized. Empty for a plain provider (the echo proof); a pylon
    /// provider carries `/lib` (CPython Lib + numpy) and `/app` (dispatcher).
    preopens: Vec<ProviderPreopen>,
    /// Number of live `instance` handles outstanding for this id. Bumped
    /// on resolve, decremented on handle drop — used to assert/log the
    /// shared-copy property (N handles, 1 resident).
    handle_count: u64,
}

/// The provider registry shared into ducklink's host store state. Holds
/// the wasm engine plus an `id -> ProviderEntry` map. Wrapped in an
/// `Arc<Mutex<..>>` so it can be cloned into a per-load store state.
#[derive(Clone)]
pub struct ProviderRegistry {
    inner: Arc<Mutex<RegistryInner>>,
}

struct RegistryInner {
    engine: Engine,
    providers: HashMap<String, ProviderEntry>,
    /// Optional digest -> id mapping for `resolve-by-digest`.
    digest_to_id: HashMap<Vec<u8>, String>,
}

impl ProviderRegistry {
    pub fn new(engine: Engine) -> Self {
        Self {
            inner: Arc::new(Mutex::new(RegistryInner {
                engine,
                providers: HashMap::new(),
                digest_to_id: HashMap::new(),
            })),
        }
    }

    /// Register a `dynlink-provider`-world wasm component under `id`,
    /// compiling it now (instantiation is deferred to first resolve).
    pub fn register_provider(&self, id: impl Into<String>, path: impl Into<PathBuf>) -> Result<(), String> {
        self.register_provider_with_preopens(id, path, Vec::new())
    }

    /// Register a `dynlink-provider`-world wasm component under `id`,
    /// with directories preopened into its OWN store on materialization.
    /// A pylon provider passes its `/lib` (CPython Lib incl. numpy) and
    /// `/app` (dispatcher pylib) here so the resident interpreter can
    /// import them; a plain provider passes an empty list.
    pub fn register_provider_with_preopens(
        &self,
        id: impl Into<String>,
        path: impl Into<PathBuf>,
        preopens: Vec<ProviderPreopen>,
    ) -> Result<(), String> {
        let id = id.into();
        let path = path.into();
        let mut inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        let bytes = std::fs::read(&path).map_err(|e| format!("read {}: {e}", path.display()))?;
        let component = Component::from_binary(&inner.engine, &bytes)
            .map_err(|e| format!("compile provider {}: {e}", path.display()))?;
        inner.providers.insert(
            id,
            ProviderEntry {
                component,
                resident: None,
                path,
                preopens,
                handle_count: 0,
            },
        );
        Ok(())
    }

    /// Map a content digest to a previously-registered id (so
    /// `resolve-by-digest` can reuse the same resident provider).
    pub fn register_digest(&self, digest: Vec<u8>, id: impl Into<String>) {
        let mut inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        inner.digest_to_id.insert(digest, id.into());
    }

    /// How many resident (instantiated-once) providers exist. Used by the
    /// integration test to assert ONE instance backs N resolves.
    pub fn resident_count(&self, id: &str) -> usize {
        let inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        inner
            .providers
            .get(id)
            .map(|e| usize::from(e.resident.is_some()))
            .unwrap_or(0)
    }

    /// How many live `instance` handles point at `id`'s resident provider.
    pub fn handle_count(&self, id: &str) -> u64 {
        let inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        inner.providers.get(id).map(|e| e.handle_count).unwrap_or(0)
    }
}

/// The per-component dynlink bridge: the shared provider registry plus a
/// dyn resource table owning the `instance` handles handed to one guest.
/// Embedded in any store state (e.g. `DotcmdState`) that wants to satisfy
/// a guest's `compose:dynlink/linker` import. The resolve/invoke logic
/// lives here so every store type delegates to ONE implementation.
pub struct DynLinkBridge {
    dyn_table: ResourceTable,
    registry: ProviderRegistry,
}

impl DynLinkBridge {
    pub fn new(registry: ProviderRegistry) -> Self {
        Self {
            dyn_table: ResourceTable::new(),
            registry,
        }
    }

    /// Shared `resolve-by-id`: materialize the resident provider ONCE
    /// (lazily) and hand back an opaque `instance` handle. See the module
    /// docs for the shared/resident model.
    pub fn resolve_by_id(&mut self, id: String) -> Result<Resource<Instance>, Error> {
        materialize_resident(&self.registry, &id)?;
        let backing = self
            .dyn_table
            .push(DynInstance {
                id,
                registry: self.registry.clone(),
            })
            .map_err(|e| err(ErrorCode::InternalError, format!("table push: {e:?}")))?;
        Ok(Resource::new_own(backing.rep()))
    }

    pub fn resolve_by_digest(&mut self, d: Vec<u8>) -> Result<Resource<Instance>, Error> {
        let id = {
            let inner = self.registry.inner.lock().unwrap_or_else(|e| e.into_inner());
            inner.digest_to_id.get(&d).cloned()
        };
        match id {
            Some(id) => self.resolve_by_id(id),
            None => Err(err(
                ErrorCode::NotImplemented,
                "resolve-by-digest: no digest->id mapping registered (use register_digest)",
            )),
        }
    }

    /// Shared `instance.invoke`: forward `method`/`payload` verbatim to the
    /// resident provider's `endpoint.handle`.
    pub fn invoke(
        &mut self,
        self_: Resource<Instance>,
        method: String,
        payload: Vec<u8>,
    ) -> Result<Vec<u8>, Error> {
        let id = self
            .dyn_table
            .get(&as_backing(&self_))
            .map_err(|e| err(ErrorCode::InternalError, format!("unknown dynlink handle: {e:?}")))?
            .id
            .clone();
        invoke_resident(&self.registry, &id, &method, &payload)
    }

    pub fn drop_handle(&mut self, rep: Resource<Instance>) -> wasmtime::Result<()> {
        let inst = self.dyn_table.delete(as_backing(&rep))?;
        let mut inner = inst.registry.inner.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(entry) = inner.providers.get_mut(&inst.id) {
            entry.handle_count = entry.handle_count.saturating_sub(1);
        }
        Ok(())
    }
}

/// Materialize (instantiate ONCE, then reuse) the resident provider for
/// `id`. Shared by every store type's `resolve`.
fn materialize_resident(registry: &ProviderRegistry, id: &str) -> Result<(), Error> {
    let mut inner = registry.inner.lock().unwrap_or_else(|e| e.into_inner());
    let RegistryInner {
        engine, providers, ..
    } = &mut *inner;
    let entry = providers
        .get_mut(id)
        .ok_or_else(|| err(ErrorCode::InvalidInput, format!("unknown provider id: {id}")))?;
    if entry.resident.is_none() {
        let mut linker: Linker<ProviderState> = Linker::new(engine);
        wasmtime_wasi::p2::add_to_linker_sync(&mut linker)
            .map_err(|e| err(ErrorCode::EmitLinkError, format!("provider wasi linker: {e}")))?;
        // Build the provider's OWN WASI ctx, preopening any registered dirs
        // (a pylon needs /lib = CPython Lib+numpy and /app = dispatcher) so
        // the resident interpreter can import them. inherit_stdio surfaces
        // the provider's init markers (e.g. "[pylon-endpoint] initializing
        // CPython interpreter (once)") to the host stderr.
        let mut builder = WasiCtxBuilder::new();
        builder.inherit_stdio();
        for po in &entry.preopens {
            builder
                .preopened_dir(&po.host, &po.guest, DirPerms::all(), FilePerms::all())
                .map_err(|e| {
                    err(
                        ErrorCode::InvalidInput,
                        format!(
                            "provider '{id}': preopen {} -> {}: {e}",
                            po.host.display(),
                            po.guest
                        ),
                    )
                })?;
        }
        let state = ProviderState {
            wasi: builder.build(),
            table: WasiResourceTable::new(),
        };
        let mut store = Store::new(engine, state);
        let instance =
            provider::DynlinkProvider::instantiate(&mut store, &entry.component, &linker)
                .map_err(|e| err(ErrorCode::ExecTrap, format!("instantiate provider '{id}': {e:?}")))?;
        entry.resident = Some(ResidentProvider { store, instance });
        eprintln!(
            "[compose-dynlink] resident provider '{id}' instantiated ONCE from {} (shared across resolves)",
            entry.path.display()
        );
    } else {
        eprintln!("[compose-dynlink] resolve '{id}' reuses the existing resident provider (1 instance)");
    }
    entry.handle_count += 1;
    Ok(())
}

/// Drive the SHARED resident provider's `endpoint.handle`. Shared by every
/// store type's `invoke`.
fn invoke_resident(
    registry: &ProviderRegistry,
    id: &str,
    method: &str,
    payload: &[u8],
) -> Result<Vec<u8>, Error> {
    let mut inner = registry.inner.lock().unwrap_or_else(|e| e.into_inner());
    let entry = inner
        .providers
        .get_mut(id)
        .ok_or_else(|| err(ErrorCode::InvalidInput, format!("provider '{id}' gone")))?;
    let resident = entry
        .resident
        .as_mut()
        .ok_or_else(|| err(ErrorCode::InternalError, format!("provider '{id}' not resident")))?;
    let endpoint = resident.instance.compose_dynlink_endpoint();
    let result = endpoint
        .call_handle(&mut resident.store, method, payload)
        .map_err(|e| err(ErrorCode::ExecTrap, format!("provider '{id}' handle trapped: {e:?}")))?;
    result.map_err(lower_provider_error)
}

/// Build a host `Error` with the given code and message.
fn err(code: ErrorCode, message: impl Into<String>) -> Error {
    Error {
        code,
        message: message.into(),
        context: None,
    }
}

/// Lower a provider-side endpoint error (a distinct generated Rust type
/// with identical shape) into the guest-facing `Error`.
fn lower_provider_error(e: provider::sys::compose::types::Error) -> Error {
    Error {
        code: ErrorCode::ExecTrap,
        message: format!("provider endpoint error: {}", e.message),
        context: e.context,
    }
}

/// The opaque handle handed to a guest as a `compose:dynlink/linker`
/// `instance` resource. It does NOT own the provider — the resident
/// provider lives in the shared registry. The handle just remembers WHICH
/// id it resolved, so `invoke` and `drop` can reach the shared instance.
pub struct DynInstance {
    id: String,
    registry: ProviderRegistry,
}

/// Host-side store state for driving a `compose:dynlink/linker` guest.
/// Carries WASI (the guest is typically a `wasi:cli/run` component) plus
/// the dyn resource table and the shared provider registry.
pub struct DynState {
    wasi: WasiCtx,
    wasi_table: ResourceTable,
    /// The dynlink bridge: shared registry + the dyn handle table.
    bridge: DynLinkBridge,
}

impl DynState {
    pub fn new(wasi: WasiCtx, wasi_table: ResourceTable, registry: ProviderRegistry) -> Self {
        Self {
            wasi,
            wasi_table,
            bridge: DynLinkBridge::new(registry),
        }
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

/// Implement the `compose:dynlink/linker` Host + HostInstance traits for a
/// store type that exposes a `&mut DynLinkBridge`. Every store type that
/// satisfies a guest's linker import (the standalone `DynState`, the
/// dot-command `DotcmdState`, a future extension store) delegates through
/// the ONE bridge implementation — no duplicated resolve/invoke logic.
#[macro_export]
macro_rules! impl_compose_dynlink_host {
    ($ty:ty, $bridge:ident) => {
        impl $crate::compose_dynlink::bindings::sys::compose::types::Host for $ty {}

        impl $crate::compose_dynlink::bindings::compose::dynlink::linker::Host for $ty {
            fn resolve_by_id(
                &mut self,
                id: ::std::string::String,
            ) -> ::core::result::Result<
                ::wasmtime::component::Resource<
                    $crate::compose_dynlink::bindings::compose::dynlink::linker::Instance,
                >,
                $crate::compose_dynlink::bindings::sys::compose::types::Error,
            > {
                self.$bridge().resolve_by_id(id)
            }

            fn resolve_by_digest(
                &mut self,
                d: ::std::vec::Vec<u8>,
            ) -> ::core::result::Result<
                ::wasmtime::component::Resource<
                    $crate::compose_dynlink::bindings::compose::dynlink::linker::Instance,
                >,
                $crate::compose_dynlink::bindings::sys::compose::types::Error,
            > {
                self.$bridge().resolve_by_digest(d)
            }
        }

        impl $crate::compose_dynlink::bindings::compose::dynlink::linker::HostInstance for $ty {
            fn invoke(
                &mut self,
                self_: ::wasmtime::component::Resource<
                    $crate::compose_dynlink::bindings::compose::dynlink::linker::Instance,
                >,
                method: ::std::string::String,
                payload: ::std::vec::Vec<u8>,
            ) -> ::core::result::Result<
                ::std::vec::Vec<u8>,
                $crate::compose_dynlink::bindings::sys::compose::types::Error,
            > {
                self.$bridge().invoke(self_, method, payload)
            }

            fn drop(
                &mut self,
                rep: ::wasmtime::component::Resource<
                    $crate::compose_dynlink::bindings::compose::dynlink::linker::Instance,
                >,
            ) -> ::wasmtime::Result<()> {
                self.$bridge().drop_handle(rep)
            }
        }
    };
}

impl DynState {
    /// Expose the bridge for the trait-impl macro.
    pub fn dynlink_bridge(&mut self) -> &mut DynLinkBridge {
        &mut self.bridge
    }
}

impl_compose_dynlink_host!(DynState, dynlink_bridge);

/// Convenience for the bindgen-generated `add_to_linker` signature
/// (`F: Fn(&mut T) -> &mut Self::Data`).
pub struct HasSelf<T>(std::marker::PhantomData<T>);
impl<T: 'static> wasmtime::component::HasData for HasSelf<T> {
    type Data<'a> = &'a mut T;
}

/// Whether a compiled component imports the `compose:dynlink/linker`
/// interface (flavor B: guest-driven dlopen). Mirrors the framework's
/// `imports_linker`. Used to conditionally add the host import so
/// components that DON'T import it are unaffected.
pub fn imports_linker(engine: &Engine, component: &Component) -> bool {
    component
        .component_type()
        .imports(engine)
        .any(|(name, _)| name.starts_with("compose:dynlink/linker"))
}

/// Add the `compose:dynlink/linker` host import to a guest linker over any
/// store type `T` that implements the linker Host traits (via
/// `impl_compose_dynlink_host!`). WASI must be added separately by the
/// caller. Used by both the standalone `DynState` path (the test) and the
/// dot-command `DotcmdState` load path.
pub fn add_to_linker<T>(linker: &mut Linker<T>) -> wasmtime::Result<()>
where
    T: bindings::compose::dynlink::linker::Host
        + bindings::compose::dynlink::linker::HostInstance
        + bindings::sys::compose::types::Host
        + 'static,
{
    bindings::DynlinkGuest::add_to_linker::<_, HasSelf<T>>(linker, |s| s)
        .map_err(|e| wasmtime::Error::msg(format!("add compose:dynlink/linker to linker: {e:?}")))
}
