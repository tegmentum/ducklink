# Ducklink Path B closure plan

**Status:** planning (2026-09-21). This is the execution plan for
closing out Path B for ducklink-host — retiring every direct
`wasmtime::*` type from ducklink-host source files, retiring
`unsafe fn primary_nested_exec`, and dropping the direct
`wasmtime` Cargo dep. Complements the migration-recipe
(`docs/wasmos-migration-recipe.md`) which tracks per-consumer
state; this doc lays out the ordered execution path.

**Prereqs (all shipped):**
- ✅ `wasmos_runtime_wasmtime_v48::SyncRuntime` + `SyncInstance` facade
  (wasmos commit `a98bc76b`).
- ✅ `wasmos_runtime_wasmtime_v48::SyncStoreState<T>` wrapper
  (wasmos commit `bedff545`).
- ✅ `wasmos_runtime_api::ReentryCapability<'a>` + `RuntimeConfig::allow_host_callback_reentry`
  (wasmos commits `0097cdf2` + `024a3b4a`).
- ✅ `wasmos_runtime_api::HostImports::register_sync`
  (already shipped).
- ✅ `#[host_iface(sync)]` typed dispatch primitive on
  `wasmos-runtime-api-macros` (already shipped).
- ✅ `CoreResourceHandle` newtype in ducklink-host
  (ducklink commit `01f7a3b`) — retires
  `wasmtime::component::ResourceAny` from 4 consumer files.

## Overview

Six phases in strict dependency order. Each phase is a coherent
unit — a partial commit inside a phase leaves the tree broken.

| Phase | Deliverable                                              | Scope     | Cascade risk | Depends on |
|-------|----------------------------------------------------------|-----------|--------------|------------|
| 1     | CoreExecution: swap store + instance types               | 1-2 days  | HIGH         | (prereqs)  |
| 2     | 5 store-construction sites → SyncRuntime                 | 2-3 days  | HIGH         | 1          |
| 3     | Host-import registrations → HostImports::register_sync   | 2-3 days  | MEDIUM       | 2          |
| 4     | Guest-export dispatch → SyncInstance::call_export        | 1-2 days  | MEDIUM       | 1, 2       |
| 5     | `primary_nested_exec` retirement (ExtensionServices break) | 3-5 days | ECOSYSTEM    | 1-4        |
| 6     | Drop direct wasmtime Cargo deps                          | 0.5 day   | LOW          | 1-5        |

**Total realistic scope:** 3-4 weeks focused ducklink-team work.
Phase 5 adds 1-2 weeks ecosystem lag for extension-author
coordination.

**Alternative pragmatic scope:** Phases 1-4 only. Ducklink keeps
`unsafe fn primary_nested_exec` (defensible pattern following
wasmtime's own reentry precedent) and keeps the direct wasmtime
Cargo dep but consumer files no longer name wasmtime types
directly. ~6-10 days.

---

## Phase 1 — CoreExecution: swap store + instance types

**Goal:** replace `Store<CoreStoreState>` and
`wasmtime::component::Instance` on `CoreExecution` (lib.rs:2765)
with their wasmos equivalents.

**Substeps:**
1. Change `CoreExecution.store` from `wasmtime::Store<CoreStoreState>`
   to `wasmos_runtime_wasmtime_v48::SyncInstance` (which owns the
   store + instance internally).
2. Delete the separate `instance:
   wasmtime::component::Instance` field — SyncInstance carries it.
3. Adapt `with_instance` and `with_core` accessor methods on
   `CoreExecution` and `CoreServices` to hand back what's needed
   from `SyncInstance` (the type-erased form; consumers use it
   via `SyncInstance::call_export` etc.).
4. Update ~30 internal call sites in lib.rs that access
   `core.store` / `core.instance` directly.
5. `CoreStoreState` moves onto `ExecutionContext::with_consumer_state`
   at instantiate time (as a `Box<dyn Any>`); handlers reach it
   via `ctx.consumer_state::<CoreStoreState>()`.

**Cascade sites:**
- `primary_nested_exec` (lib.rs:10343) reads
  `*mut Store<CoreStoreState>` — must be redesigned OR retired in
  Phase 5. Phase 1 keeps the raw pointer as-is temporarily
  (still unsafe, still works) so Phase 1 can land without also
  landing Phase 5.

**Rollback:** revert the type swap; internal call sites revert too.
Single-commit-per-phase discipline.

**Testing:** consumer test suites (icd-9, icd-10) unchanged; ducklink
own test suite runs. Test failures due to missing pre-built
`ducklink_core.wasm` artifacts are pre-existing environmental
issues; those failures should be no worse than the current baseline
(30 known env failures).

---

## Phase 2 — 5 store-construction sites → SyncRuntime

**Goal:** every `let mut store = Store::new(&engine, ...)` becomes
a `SyncRuntime::instantiate(compiled, ctx)`.

**Sites** (from `git grep 'Store::new' crates/ducklink-host/src/lib.rs`):
- **lib.rs:3668** — DotcmdRegistry::load_one builds a
  `Store<DotcmdState>`. Migrates to `SyncRuntime` + wraps
  DotcmdState via `with_consumer_state`.
- **lib.rs:~10702** — `open_driver_core_with_bootstrap` — the
  main DuckDB core construction. Consumers of `DriverConnection`
  cascade through Phase 1.
- **lib.rs:~11934** — core-with-cli construction. Similar
  shape to open_driver_core.
- **lib.rs:~12105** — standalone-shell path.
- **lib.rs:~12262** — cli host_state construction.

**Config translation** — each site currently builds
`wasmtime::Config` + `WasiCtxBuilder` inline. Refactor to a shared
`build_runtime_config()` fn returning `RuntimeConfig`, then per-site
`WasiEnvironment` construction.

**`build_engine` retires** — the shared wasmtime engine builder
(lib.rs:11140) becomes a `SyncRuntime` factory. All engine-config
knobs (fuel, epoch, cache, wasm features) map to `RuntimeConfig`.

**Cascade:** any use of the raw `wasmtime::Engine` outside the
factory (e.g. `DriverStoreState.engine: Engine` still used for
per-connection DuckDB cores) either migrates or picks up an
adapter-native engine handle via `SyncRuntime`.

**Rollback:** per-site — each construction site is independently
migratable, though they SHARE the engine builder so
`build_engine` retirement blocks partial rollback.

---

## Phase 3 — Host-import registrations → HostImports::register_sync

**Goal:** every `sync_bridge_resource::install_host_call<S>` /
`sync_bridge::install_stateless_host_call<S>` call becomes
`HostImports::new().register_sync(iface, handler)` attached to
the `ExecutionContext` at instantiate time.

**Sites** (from `git grep 'install_host_call\|install_stateless_host_call' crates/ducklink-host/src/lib.rs`):
- **callback-dispatch** (`CallbackDispatchHost` at ~lib.rs:1112)
- **tvm-manager** + **tvm-bytes**
- **host-extension-loader** + **extension-loader-hooks**
- **duckdb:dotcmd/spi** (lib.rs:3634)

The `SyncHostCall` impls themselves stay; only the registration
surface changes. Also opportunity to migrate each to
`#[host_iface(sync)]` typed dispatch as part of the same site
(mechanical follow-up).

**Depends on Phase 2** — HostImports attaches to an
`ExecutionContext` used by `SyncRuntime::instantiate`; requires
the store to be `Store<AdapterHostState>` (wasmos-native), which
Phase 2 delivers.

---

## Phase 4 — Guest-export dispatch → SyncInstance::call_export

**Goal:** every `sync_export_bridge::call_export*` call in lib.rs
becomes `SyncInstance::call_export`.

**Sites** (~30+ in lib.rs):
- `call_database_returning_resource_on_core`
- `call_database_execute_on_core`
- `call_export_on_resource_core`
- `call_export_unit_result_on_core`
- `call_export_on_resource`
- `call_export_unit_result`
- Multiple test bodies (lib.rs:~13667+)

**Resource marshalling** — where the guest export takes/returns
a resource, prefer typed `Resource<T>` args over
`ExportResourceTable` + `call_export_with_resources`. This
matches the pattern icd-9 / icd-10 / ducklink driver_exec already
use post-`#[host_iface(sync)]` adoption.

**Depends on Phases 1 + 2** — the Instance held on `CoreExecution`
is now a `SyncInstance`, and its `call_export` is what these
call sites reach.

---

## Phase 5 — `primary_nested_exec` retirement (cross-crate)

**Goal:** retire `unsafe fn primary_nested_exec` +
`PrimaryReentryGuard` + `PRIMARY_STORE_REENTRY` TLS. Replace with
`HostCallContext::reentry()?.call_export(...)` inside the
callback-dispatch chain.

**Cross-crate scope:**

1. **ducklink-runtime — SEMVER-MAJOR trait break.**
   `pub trait ExtensionServices` at `extension.rs:533` gets a
   `ctx` parameter on `nested_exec`:
   ```rust
   fn nested_exec(
       &mut self,
       ctx: &mut wasmos_runtime_api::HostCallContext<'_>,
       sql: &str,
   ) -> Result<NestedExecResult, String>;
   ```
2. **ducklink-runtime — every `impl ExtensionServices`** updates.
   Search (`git grep 'fn nested_exec' crates/ducklink-runtime/src/`):
   - `extension.rs:2126` (production impl)
   - Native-DuckDB direction impls
   - Test impls at 6081, 6118, 6133, 6137, 6150
3. **ducklink-host — CallbackDispatchHost + friends** thread the
   `ctx: &mut HostCallContext<'_>` down through
   `extension_manager.dispatch_call_scalar(...)` etc. The
   `ExtensionManager` methods gain the same ctx parameter.
4. **Runtime config gate:** ducklink-host's runtime builder sets
   `RuntimeConfig::allow_host_callback_reentry = true` (trades
   streams / futures / threading — already documented in the
   migration recipe).
5. **Retire the scaffolding:** delete lib.rs:3010-3047 (TLS +
   guard) and lib.rs:10343 (unsafe fn).
6. **Third-party extension coordination:** any extension using
   ducklink's SDK's `ExtensionServices` needs the trait update.
   Announce + release plan needed.

**Estimated scope:** 3-5 days ducklink-side surgery. 1-2 weeks
ecosystem lag for external extensions.

**Alternative if Phase 5 blocks:** keep `unsafe fn
primary_nested_exec` as the defensible pattern it already is —
it follows wasmtime's own re-entrancy precedent (see
`tests/reentrancy_poc.rs`). Phase 6 still lands so long as no
other direct wasmtime type usage remains.

---

## Phase 6 — Drop direct wasmtime Cargo deps

**Goal:** retire from `crates/ducklink-host/Cargo.toml`:
```toml
wasmtime = "48.0.0"
wasmtime-wasi = "48.0.0"
wasmtime-wasi-http = "48.0.0"
```

All three become transitive dependencies of
`wasmos-runtime-wasmtime-v48`.

**Cost:** half day if Phases 1-5 are clean. Any stray
`use wasmtime::…` or `.wasmtime_method()` surfaces as a compile
error.

**Verification:** `cargo tree -p ducklink-host --edges no-normal
--depth 2 | grep wasmtime` shows only transitive edges.

---

## Testing strategy

- **Per-phase test gate:** consumer suites (icd-9: 70 tests,
  icd-10: 58 tests) must stay green throughout. Ducklink-host
  own tests: track baseline (~128 passing, 30 env-blocked); no
  new red.
- **CI parity:** the 30 env-blocked tests need `ducklink_core.wasm`
  built. Migration validation on a machine with the artifact
  should confirm behavioral parity.
- **Reentry integration test:** once Phase 5 lands, add an end-to-end
  test that (a) opens a DuckDB core with reentry config,
  (b) registers a scalar callback that itself issues SQL via
  `ctx.reentry()`, (c) verifies the nested query lands on the
  primary catalog. Mirrors the existing
  `phase4_fu4_sibling_replay_archive_populated_from_primary_drain`
  shape.

## Rollback strategy per phase

| Phase | Rollback                                             |
|-------|------------------------------------------------------|
| 1     | Revert type swap; ~30 call sites revert too         |
| 2     | Per-site — each construction site can revert alone (except shared `build_engine`) |
| 3     | Per-site — sync_bridge_resource still works as escape hatch |
| 4     | Per-site — sync_export_bridge still works as escape hatch |
| 5     | Cannot rollback once trait signature changes (semver-major); only forward |
| 6     | Cargo.toml revert if any stray wasmtime use surfaces |

## Milestone announcement (post-Phase 5)

Wasmos side: no code changes needed. Migration status doc
(`~/git/wasmos/docs/design/runtime-abstraction/state-of-the-abstraction.md`)
gains a "Ducklink Path B FULLY CLOSED" entry.

Ducklink side: SEMVER-MAJOR release of ducklink-runtime; changelog
notes the `ExtensionServices::nested_exec` signature change and
provides a migration example.

Extension authors: announcement + migration guide + provided
`ReentryCtx` helper if it turns out to need shape polish based on
real extension migration experience.
