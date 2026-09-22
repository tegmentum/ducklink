# Path B closure — Slice 3 mechanical execution guide

**Companion to** [`docs/path-b-closure-plan.md`](path-b-closure-plan.md).
**Prerequisites landed:** Phase 1a-1e, Slices 1+2, wasmos Phase 6.24
(`SyncCrossInstanceHandle` — wasmos commits `958d5d66`, `d25b2e67`).

This document lists the concrete edits Slice 3 requires, in
execution order, with file paths and approximate line numbers,
so a fresh session can execute mechanically without re-deriving
scope. The refactor is genuinely atomic — see the plan doc's
fusion note for why — so a "partial landing" is not a valid stop
point. Roll back to a snapshot and try again rather than commit
an intermediate broken state.

## Preflight

1. **Snapshot:** `git status` — ducklink working tree must be
   clean. Note the current HEAD SHA as the rollback point.
2. **Baseline tests:** `cargo test -p ducklink-host --lib 2>&1 |
   tail -5` — record pass/fail counts to compare after the
   surgery.
3. **wasmos side:** verify `wasmos-runtime-wasmtime-v48` has
   `SyncCrossInstanceHandle` exported (grep the crate; if the
   commit hash `d25b2e67` isn't in `git log`, sync wasmos first).

## Edit sequence

### 1. Wasmos-facing imports (top of `lib.rs`)

Add to the existing wasmos imports:

```rust
use wasmos_runtime_api::{
    // ...existing...
    CompileOptions, ComponentSource, ExecutionContext, HostImports,
    RuntimeConfig,
};
use wasmos_runtime_wasmtime_v48::{
    // ...existing...
    SyncCrossInstanceHandle, SyncInstance, SyncRuntime,
    WasmtimeV48Runtime,
    sync_export_bridge::ExportResourceTable,
};
```

### 2. `CoreExecution` struct definition (~line 2868)

```rust
// BEFORE
struct CoreExecution {
    store: Store<CoreStoreState>,
    instance: wasmtime::component::Instance,
}

// AFTER
struct CoreExecution {
    /// The wasmos-native synchronous facade over the core wasm
    /// instance. Owns the underlying wasmtime store + component
    /// instance internally. Every guest-export dispatch routes
    /// through `sync_inst.call_export` or (for cross-instance
    /// reentry) `SyncCrossInstanceHandle` snapshotted from this.
    sync_inst: SyncInstance,
}
```

### 3. `instantiate_core` rewrite (~line 10787)

Replace the entire function body. New shape:

```rust
fn instantiate_core(
    engine: &Engine,
    component_path: &Path,
    wasi_env: wasmos_runtime_api::WasiEnvironment,
    extension_manager: Arc<Mutex<ExtensionManager>>,
) -> Result<CoreExecution> {
    let bytes = std::fs::read(component_path).with_context(|| {
        format!("read core component at {}", component_path.display())
    })?;

    // Reuse ducklink's existing engine (from build_engine /
    // build_engine_for_driver) via WasmtimeV48Runtime::from_engine.
    // Default RuntimeConfig — cross-instance sync reentry works
    // under the default async component-model config per Phase
    // 6.24's default-config parity finding.
    let inner_rt = WasmtimeV48Runtime::from_engine(
        engine.clone(),
        RuntimeConfig::default(),
    )
    .map_err(|e| anyhow::anyhow!("build WasmtimeV48Runtime: {e:?}"))?;
    let sync_rt = SyncRuntime::from_runtime(inner_rt)
        .map_err(|e| anyhow::anyhow!("build SyncRuntime: {e:?}"))?;

    let compiled = sync_rt
        .compile_component(
            ComponentSource::Bytes {
                bytes: bytes.into(),
                name: Some(component_path.display().to_string()),
            },
            CompileOptions::default(),
        )
        .map_err(|e| anyhow::anyhow!("compile core component: {e:?}"))?;

    // All five host imports now register through wasmos-native
    // HostImports::register_sync. The handler downcasts flip
    // from ctx.consumer_state::<CoreStoreState>() to
    // ctx.consumer_state::<CoreInnerState>() (see step 6).
    let imports = HostImports::new()
        .register_sync(TVM_BYTES_IFACE, TvmBytesHost)
        .register_sync(TVM_MANAGER_IFACE, TvmManagerHost)
        .register_sync(HOST_EXTENSION_LOADER_IFACE, CoreHostExtensionLoaderHost)
        .register_sync(EXTENSION_LOADER_HOOKS_IFACE, ExtensionLoaderHooksHost)
        .register_sync(CALLBACK_DISPATCH_IFACE, CallbackDispatchHost);

    // Consumer state is now the raw CoreInnerState — the
    // SyncStoreState<CoreInnerState> wrap is retired (Slice
    // 2+3+4.5 does the final cleanup; here we just stop wrapping).
    let inner_state = CoreInnerState {
        extension_manager,
        tvm: tvm_core::RegionDirectory::new(),
        tvm_slots: std::collections::HashMap::new(),
        replay_archive: None,
        is_sibling: false,
    };

    let ctx = ExecutionContext::new()
        .with_wasi(wasi_env)
        .with_host_imports(imports)
        .with_consumer_state(inner_state);

    let sync_inst = sync_rt
        .instantiate(&compiled, ctx)
        .map_err(|e| anyhow::anyhow!("instantiate core: {e:?}"))?;

    Ok(CoreExecution { sync_inst })
}
```

Cascade: `SyncRuntime` is dropped when `sync_inst` is dropped
(the tokio runtime is `Arc`-shared via `SyncInstance`). Verify
by grep — no `Arc<SyncRuntime>` field needs to survive on
`CoreExecution` unless a caller does `compile_component` after
instantiation.

### 4. `CoreExecution` methods migration (~line 2915)

Retire the four helper wrappers that go through the escape-hatch
free functions. Under `SyncInstance`, dispatch is direct.

```rust
// BEFORE — call_bridge_export at ~line 2967
pub(crate) fn call_bridge_export(
    &mut self,
    iface: &str,
    method: &str,
    args: &[wasmos_runtime_api::Value],
) -> Result<Vec<wasmos_runtime_api::Value>, wasmos_runtime_api::RuntimeError> {
    use wasmos_runtime_wasmtime_v48::sync_export_bridge::{
        call_export_with_resources, ExportResourceTable,
    };
    let mut resources = ExportResourceTable::new();
    call_export_with_resources(
        self.store.as_context_mut(),
        &self.instance,
        Some(iface),
        method,
        args,
        &mut resources,
    )
}

// AFTER
pub(crate) fn call_bridge_export(
    &mut self,
    iface: &str,
    method: &str,
    args: &[wasmos_runtime_api::Value],
) -> Result<Vec<wasmos_runtime_api::Value>, wasmos_runtime_api::RuntimeError> {
    self.sync_inst.call_export(&format!("{iface}#{method}"), args)
}
```

For `resource_drop_handle` at ~line 2990, wasmos now exposes
`SyncInstance::resource_drop(store_id, handle_id)` (wasmos
commit `9a7c61ea`). If `CoreResourceHandle` carries the
wasmos-native `(store_id, handle_id)` fields:

```rust
// AFTER
pub(crate) fn resource_drop_handle(
    &mut self,
    handle: CoreResourceHandle,
) -> Result<(), wasmos_runtime_api::RuntimeError> {
    self.sync_inst.resource_drop(handle.store_id, handle.handle_id)
}
```

Return type changes from `wasmtime::Result<()>` to
`Result<(), RuntimeError>` — one small consumer-side adjustment
at the ~5 drop sites in `lib.rs` + consumer files (grep for
`resource_drop_handle`).

### 5. Free helper functions (`call_export_on_resource_core`
etc.) — RETIRE

The four free functions at line 9696-9902
(`call_export_on_resource_core`, `call_database_returning_resource_on_core`,
`call_database_execute_on_core`, `call_export_unit_result_on_core`)
all use `core.store.as_context_mut()` + `&core.instance` + the
escape-hatch bridge. Their consumers (in the `impl CoreExecution`
block and elsewhere in lib.rs) go directly through `sync_inst`
under Slice 3.

**Two options:**

- **Option A (recommended)**: Retire the free functions. Migrate
  each caller in-place to `self.sync_inst.call_export(iface #
  method, args)` with args including any `Value::Resource`
  entries.
- **Option B**: Reshape the free functions to accept
  `&mut SyncInstance` instead of `&mut CoreExecution` and use
  `sync_inst.call_export(...)` internally. Simpler cascade but
  keeps helper indirection.

Option A is cleaner. Callers are at ~10 sites in lib.rs — grep
for the four function names.

### 6. Host handler downcast flip

The 25 sites migrated onto the `CoreState` trait in Slice 2 all
call `ctx.consumer_state::<CoreStoreState>()`. Under Slice 3
these flip to `ctx.consumer_state::<CoreInnerState>()`.

Mechanical replace:

```bash
# In lib.rs
sed -i '' 's/consumer_state::<CoreStoreState>/consumer_state::<CoreInnerState>/g' \
    crates/ducklink-host/src/lib.rs
```

Then verify: `grep -c 'consumer_state::<CoreStoreState>' lib.rs`
must return 0.

Handler bodies stay unchanged — Slice 2's `CoreState` trait is
implemented for both `SyncStoreState<CoreInnerState>` AND
`CoreInnerState` (see the impl blocks near `CoreState` trait
definition), so trait-method calls work either way. This is
exactly why Slice 2 landed as prep.

### 7. `CoreResourceHandle` migration (~line 2901)

Gap closed by wasmos `daeaed79`. Adopt the single-field
wasmos-native shape:

```rust
// AFTER
#[derive(Clone, Copy, Debug)]
pub(crate) struct CoreResourceHandle {
    pub(crate) store_id: u64,
    pub(crate) handle_id: u64,
}

impl CoreResourceHandle {
    pub(crate) fn as_value(self) -> wasmos_runtime_api::Value {
        wasmos_runtime_api::Value::Resource {
            store_id: self.store_id,
            handle_id: self.handle_id,
        }
    }

    pub(crate) fn from_value(v: &wasmos_runtime_api::Value)
        -> Option<Self>
    {
        match v {
            wasmos_runtime_api::Value::Resource { store_id, handle_id } => {
                Some(Self { store_id: *store_id, handle_id: *handle_id })
            }
            _ => None,
        }
    }
}
```

No `wasmtime::component::ResourceAny` field. Consumers hold
`CoreResourceHandle`, convert to `Value::Resource` at dispatch
time (both in-instance and cross-instance paths accept the
wasmos-native shape).

### 8. `primary_nested_exec` rewire (~line 10499)

```rust
// BEFORE
#[derive(Clone, Copy)]
struct PrimaryReentry {
    store: *mut Store<CoreStoreState>,
    instance: *const wasmtime::component::Instance,
    connection: ResourceAny,
}

unsafe fn primary_nested_exec(
    reentry: PrimaryReentry,
    sql: &str,
) -> Result<NestedExecResult, String> {
    let instance = unsafe { &*reentry.instance };
    let store: &mut Store<CoreStoreState> = unsafe { &mut *reentry.store };
    let mut resources = ExportResourceTable::new();
    let conn_val = resources.register(reentry.connection);
    let ret = call_export_with_resources(
        store.as_context_mut(),
        instance,
        Some(DATABASE_IFACE),
        "execute",
        &[conn_val, Value::String(sql.to_string())],
        &mut resources,
    )
    // ...
}

// AFTER — no wasmtime types anywhere
#[derive(Clone, Copy)]
struct PrimaryReentry {
    handle: SyncCrossInstanceHandle,
    connection: CoreResourceHandle,
}

unsafe fn primary_nested_exec(
    reentry: PrimaryReentry,
    sql: &str,
) -> Result<NestedExecResult, String> {
    let mut handle = reentry.handle;
    // SAFETY: HostState::execute installs this reentry via
    // PrimaryReentryGuard::set immediately before the outer
    // call_execute. The core SyncInstance is quiescent from this
    // reentry's perspective (the outer call is running on target
    // = extension SyncInstance, not source = core). Same OS
    // thread guaranteed by wasmtime's sync callback dispatch.
    let ret = unsafe {
        handle.call_export_via_store(
            &format!("{DATABASE_IFACE}#execute"),
            &[reentry.connection.as_value(), Value::String(sql.to_string())],
        )
    }
    .map_err(|e| format!("nested-exec: primary call_execute trapped: {e}"))?;
    // ...result-unpack unchanged...
}
```

### 9. `PrimaryReentryGuard::set` call sites (~line 9219, ~line 15500)

```rust
// BEFORE (in HostState::execute)
let store_ptr: *mut Store<CoreStoreState> = &mut core.store;
let instance_ptr: *const wasmtime::component::Instance = &core.instance;
let _reentry = PrimaryReentryGuard::set(PrimaryReentry {
    store: store_ptr,
    instance: instance_ptr,
    connection: entry_handle,
});

// AFTER
// SAFETY: the core SyncInstance is idle at this point — this
// runs BEFORE the outer call_execute on it. Cross-instance
// dispatch via the returned handle happens from inside the
// extension's SyncInstance callback, so the source (core) is
// quiescent then too.
let handle = unsafe { core.sync_inst.cross_instance_reentry_handle() };
let _reentry = PrimaryReentryGuard::set(PrimaryReentry {
    handle,
    connection: entry_handle,
});
```

Same edit at the test-side site (~line 15500).

### 10. `DriverCoreState` migration (~line 11111)

```rust
// BEFORE
pub(crate) struct DriverCoreState {
    core: Arc<Mutex<CoreExecution>>,
    connection: wasmtime::component::ResourceAny,
    _extension_manager: Arc<Mutex<ExtensionManager>>,
}

// AFTER — depends on step 7's CoreResourceHandle shape
pub(crate) struct DriverCoreState {
    core: Arc<Mutex<CoreExecution>>,
    connection: CoreResourceHandle, // or keep ResourceAny if 7 uses dual-field
    _extension_manager: Arc<Mutex<ExtensionManager>>,
}
```

### 11. `open_driver_core_with_bootstrap` + CLI paths + sibling
paths (~line 11157, 12099, 12285, 12440)

Each site currently builds `let mut store = Store::new(engine,
...)` then `instance = pre.instantiate(store)`. Under Slice 3,
each collapses into `let sync_inst = sync_rt.instantiate(...)`.

For the paths that carry a bespoke consumer state (`DotcmdState`,
shell state, host state), the shape is:

```rust
let ctx = ExecutionContext::new()
    .with_wasi(wasi_env)
    .with_host_imports(imports)
    .with_consumer_state(state);
let sync_inst = sync_rt.instantiate(&compiled, ctx)?;
```

Same as `instantiate_core`. Each site becomes ~15-25 lines of
mechanical replacement.

### 12. Engine builder cascade (~line 11097 + 11294)

`build_engine_for_driver()` currently returns `wasmtime::Engine`.
Two paths:

- **Path A**: keep `build_engine_for_driver` returning Engine.
  Every SyncRuntime construction call wraps it via
  `WasmtimeV48Runtime::from_engine(engine.clone(), config)`.
- **Path B**: retire `build_engine_for_driver`, replace with
  `build_sync_runtime_for_driver()` returning `SyncRuntime`.
  Cleaner but touches more callers.

Path A is smaller. Path B is what Phase 6 (Cargo dep drop) will
eventually need. Recommend Path A for Slice 3, then Path B in
Slice 3+4+.5.

### 13. Consumer files (`replicate.rs`, `ui_server.rs`,
`quack_server.rs`, `httpd.rs`)

These files already migrated onto `CoreResourceHandle` (Phase
1a). If step 7 keeps `CoreResourceHandle` as `Copy` with the same
public surface (`call_database_returning_handle`,
`execute_on_handle`, `call_export_on_handle`,
`call_bridge_export`, `resource_drop_handle`), consumer files
require **zero changes** — that was the whole point of Phase 1a.

Verify by grep: `grep -rn 'wasmtime::' crates/ducklink-host/src/{replicate,ui_server,quack_server,httpd}.rs`
must return zero results after Slice 3.

### 14. Compile + iterate

```bash
cd crates/ducklink-host
cargo check 2>&1 | head -50
```

Expected first-pass errors:

- Many sites still name `Store<CoreStoreState>` — find via grep,
  replace with SyncInstance-flavour access.
- Any handler downcast site missed by step 6's sed.
- `resource_drop_handle` implementation gap (step 4's TODO).
- Sibling core construction path in `sibling_ensure_slot` (~line
  10547) — same shape as `instantiate_core`.

Iterate until `cargo check` clean. Then `cargo build`. Then
`cargo test --lib -- --test-threads=1 2>&1 | tail -20`.

Target baseline: ~128 passing, ~30 env-blocked (missing
`ducklink_core.wasm` build). No net new red.

### 15. Commit

Single commit message shape:

```
refactor(ducklink-host): flip CoreExecution to SyncInstance (Slice 3)

Retires the escape-hatch bridge as the primary dispatch path.
CoreExecution now holds a wasmos-native SyncInstance; the raw
Store<CoreStoreState> and wasmtime::component::Instance fields
are gone.

- CoreExecution.{store, instance} → sync_inst: SyncInstance
- instantiate_core rewritten via SyncRuntime::instantiate
- 5 host imports migrate to HostImports::register_sync
- 25 handler downcasts flip <CoreStoreState> → <CoreInnerState>
- 4 escape-hatch free functions retired; callers dispatch via
  sync_inst.call_export directly
- primary_nested_exec rewires PrimaryReentry to hold a
  SyncCrossInstanceHandle (wasmos Phase 6.24) instead of raw
  *mut Store + *const Instance pointers
- CoreResourceHandle keeps its Copy shape; carries both the
  wasmos Value::Resource form and (transitionally) the wasmtime
  ResourceAny for cross-instance dispatch via ExportResourceTable

Consumer files (replicate.rs, ui_server.rs, quack_server.rs,
httpd.rs) unchanged — CoreResourceHandle + CoreExecution methods
kept the same public shape.

Path B closure — Slices 2+3+4.5 (SyncStoreState wrap retirement)
and Phase 6 (Cargo dep drop) remain as follow-up cleanup.
```

## Known partial-retirement gaps — ALL CLOSED (2026-09-22)

All three gaps flagged at the design stage were closed with
wasmos-side additions before Slice 3 execution began:

1. ~~`PrimaryReentry.connection: wasmtime::component::ResourceAny`~~
   **CLOSED** (wasmos `daeaed79`). Use
   `SyncCrossInstanceHandle::call_export_via_store(name, args)`
   with `Value::Resource` entries in `args`; wasmos looks the
   resource up via the source's per-instance
   `AdapterHostState` table. `PrimaryReentry.connection` becomes
   a plain `Value::Resource` — no wasmtime type.
2. ~~`CoreResourceHandle` dual-field shape~~ **CLOSED** — same
   underlying wasmos primitive. `CoreResourceHandle` wraps
   `Value::Resource` (or `(store_id, handle_id)` fields) only.
3. ~~`resource_drop_handle`~~ **CLOSED** (wasmos `9a7c61ea`).
   Use `sync_inst.resource_drop(store_id, handle_id)`.

**Consequence:** Slice 3 lands with **zero** internal uses of
`wasmtime::component::ResourceAny` on the consumer surface —
including on the cross-instance dispatch path. Phase 6's Cargo
dep drop becomes a pure remove-the-imports mechanical change.

## Verification checklist

Before committing:

- [ ] `cargo check -p ducklink-host` clean
- [ ] `cargo build -p ducklink-host` clean
- [ ] `cargo test -p ducklink-host --lib -- --test-threads=1`
      passes at or above baseline
- [ ] `grep -c 'consumer_state::<CoreStoreState>' crates/ducklink-host/src/lib.rs`
      returns 0
- [ ] `grep -rn 'wasmtime::' crates/ducklink-host/src/{replicate,ui_server,quack_server,httpd}.rs`
      returns 0
- [ ] Sibling core construction path (`sibling_ensure_slot`) also
      migrated
- [ ] `git log --oneline HEAD~1..HEAD` shows exactly one Slice 3
      commit

If any checkbox fails, DO NOT commit. Roll back to the snapshot
SHA and re-plan.
