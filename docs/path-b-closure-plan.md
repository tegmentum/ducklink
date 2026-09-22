# Ducklink Path B closure plan

**Status (2026-09-22): PARTIAL.** All Path B closure phases
through CliHarness migration have landed. Phase 6 (wasmtime
Cargo dep drop) is blocked on three cross-crate migrations
documented at the bottom of the phase table.

- Slice 3 atomic surgery landed (`ad3156a`): CoreExecution flipped
  to wasmos-native SyncInstance; `primary_nested_exec` rewired
  onto Phase 6.24's cross-instance sync reentry primitive.
- Standalone-shell driver migrated (`4f56c37`).
- ExtensionManager `wasmtime::Result` error interop retired to
  `anyhow::Result` (`0b0a302`).
- CliHarness + run_cli_inner + wire_cli_bridged_host_imports +
  dispatch_cli_run all migrated (`1ac637c`).
- Slice 3 external-consumer regression fixed (`39ac622`,
  wasmos `88d45bc1`) — `SyncInstance::call_export_reentrant`
  primitive prevents nested-tokio panics for consumers embedding
  ducklink inside their own SyncRuntime.
- **Phase 5 retired from the plan** — Phase 6.24 (wasmos
  cross-instance sync reentry primitive) makes the semver-major
  `ExtensionServices` trait break unnecessary.
- Phase 6 blockers documented (`b45bfe8`) — one remaining after
  `bc39d9f` migrated DotcmdInstance to SyncInstance: the
  ExtensionManager Engine cascade via `build_engine_for_driver`
  (est. 5-8 sessions in ducklink-runtime + ducklink-host).
  DotcmdInstance retirement has one documented degradation:
  dot-command components that import `compose:dynlink/linker`
  (pylon-shaped dot-commands) are skipped by
  `DotcmdRegistry::load_one` with a warning; adding the
  wasmos-native install path is tracked with the joint
  compose_dynlink migration.

`wasmtime::` count in `crates/ducklink-host/src/lib.rs`: **19**
(down from ~110 at Slice 2 start). Breakdown: 2 real-code
imports, 3 real-code type/method sites, ~14 archaeological
comments preserved.

This is the execution plan for closing out Path B for
ducklink-host — retiring every direct `wasmtime::*` type from
ducklink-host source files, retiring `unsafe fn
primary_nested_exec` (DONE), and dropping the direct `wasmtime`
Cargo dep (PENDING). Complements the migration-recipe
(`docs/wasmos-migration-recipe.md`) which tracks per-consumer
state; this doc lays out the ordered execution path.

**Phase 1 preparatory landings (2026-09-21):**
- `fd879cc` — `CoreExecution::call_bridge_export` +
  `resource_drop_handle` methods encapsulate the escape-hatch
  bridge; 3 consumer files (quack_server, ui_server, httpd) no
  longer name `wasmtime::component::Instance` or
  `StoreContextMut` at call sites.
- `9ff6bf5` — 5 config/logging dispatch sites inside
  `impl ExtensionServices for CoreServices` migrated onto
  `call_bridge_export`; retires the last `with_instance` callers
  in lib.rs.
- `73ea896` — 2 extension-management dispatch sites
  (register_extension, list_registered_extensions) migrated.
- **Result:** no consumer-facing wasmtime types outside lib.rs's
  internal helpers; store/instance access is fully encapsulated
  behind `CoreExecution` methods. Sets up the actual Phase 1
  type-swap (7 `.store` / `.instance` internal accessors on
  `CoreExecution` and 5 `Store::new` construction sites) as a
  contained refactor.

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
Phase 1 is further split into five substeps (1a-1e) to reflect the
2026-09-21 execution reality: three preparatory sub-slices and one
structural sub-slice landed before the type-swap itself, so
progress accumulates in committable chunks instead of one big-
bang commit.

| Phase | Deliverable                                              | Scope     | Cascade risk | Status |
|-------|----------------------------------------------------------|-----------|--------------|--------|
| 1a    | Encapsulate escape-hatch bridge in CoreExecution methods | 0.5 day   | LOW          | ✅ 2026-09-21 (fd879cc + 9ff6bf5 + 73ea896) |
| 1b    | Retire dead HasData impls                                 | 0.1 day   | LOW          | ✅ 2026-09-21 (00d5c51) |
| 1c    | Wasmos gap: `Instance::consumer_state_mut`                | 0.5 day   | LOW          | ✅ 2026-09-21 (wasmos d05cb2a3) |
| 1d    | Extract `CoreInnerState` from `CoreStoreState`            | 0.5 day   | MEDIUM       | ✅ 2026-09-21 (7c41ce8) |
| 1e    | Flip `CoreStoreState` → `SyncStoreState<CoreInnerState>`  | 1 day     | MEDIUM       | ✅ 2026-09-22 (5f3b1e8) |
| 2+3+4 | Combined arc — split into 5 Path 2 slices below           | 5 sessions | HIGH        | IN PROGRESS 2026-09-22 |
| 2+3+4.1 | `CoreState` accessor trait (helper interface layer)     | ~2 hrs    | LOW          | ✅ 2026-09-22 (aae7e12) |
| 2+3+4.2 | Migrate 25 handler sites onto `CoreState` trait         | ~1 hr     | LOW          | ✅ 2026-09-22 (d07469e) |
| 2+3+4.3 | Swap `CoreExecution` to `SyncInstance`; migrate to `HostImports::register_sync`; flip consumer_state box; rewire `primary_nested_exec` onto wasmos Phase 6.24 primitive | ~1-2 wks | HIGH | ✅ 2026-09-22 (`ad3156a`) |
| 2+3+4.4 | (merged into 2+3+4.3 — see arc note)                    |            |             | N/A |
| 2+3+4.5 | Retire `SyncStoreState<CoreInnerState>` wrap from CoreExecution path | ~2 hrs | LOW | ✅ 2026-09-22 (`a79c6f6` initial; fully retired in `4f56c37` after shell-driver follow-up migration) |
| 5     | `ExtensionServices` semver-major trait break — NO LONGER NEEDED | — | — | RETIRED (Phase 6.24 makes ctx-threading unnecessary — nested_exec keeps its `&mut self, sql` signature, internally holds a `SyncCrossInstanceHandle` and dispatches through it) |
| 6     | Drop direct wasmtime Cargo deps                          | multi-arc | MEDIUM | BLOCKED on ExtensionManager only — CliHarness ✅ (`1ac637c`), standalone-shell ✅ (`4f56c37`), compile-cache restored on wasmos-native paths ✅ (`39a5525`), DotcmdInstance ✅ (`bc39d9f` with documented degradation for pylon dot-cmds that import `compose:dynlink/linker`). Remaining: ExtensionManager Engine cascade via `build_engine_for_driver` (est. 5-8 sessions). Full details: agent commit `b45bfe8`. |

**Fusion note (2026-09-22 discovery):** Slice 2+3+4.3 originally
scoped to keep `primary_nested_exec`'s raw-pointer TLS pattern
intact. Investigation revealed reentry cannot use
`sync_inst.call_export` (nested `block_on` on the tokio executor
thread deadlocks — the exact problem `call_export_sync` was added
to solve) and `ReentryCapability<'a>` cannot be stashed in TLS
(lifetime-bound). So reentry has to reach the callback's `ctx`
on-stack.

**Follow-up discovery (2026-09-22, same session):** the ctx-based
approach doesn't work either — because ducklink's
`primary_nested_exec` is a CROSS-INSTANCE reentry, not
same-instance. Trace of the actual flow:

- Layer 1: `HostState::execute` → `core.sync_inst.call_export(execute)`
- Layer 2: core guest → `callback-dispatch` host import →
  `extension_manager.dispatch_scalar(…)` → the extension's
  `ext_inst.call_export(call-scalar)` — a DIFFERENT SyncInstance
  than layer 1
- Layer 3: extension guest → `duckdb:extension/nested-exec` host
  import → `state.services.nested_exec(sql)` [`CoreServices` impl]
  → wants to run SQL on the CORE (layer 1's instance)

The `ctx` inside layer 3's host handler reenters the EXTENSION
instance (layer 2's), not the CORE. `ReentryCapability` documents
this explicitly: "Only reaches back into the SAME instance that
dispatched this host handler. Reentry across instances is a
separate future primitive."

**Consequence:** Path B "fully closed" (retire raw-pointer TLS)
requires either:

A. **Wasmos-side cross-instance sync reentry primitive.** A real
   design + implementation project — probably an unsafe raw-store
   accessor from `SyncInstance` (e.g.
   `unsafe fn primary_store_ptr()`) surfaced as a supported
   wasmos primitive, plus a matching sync_export_bridge-flavour
   dispatch that works when the outer instance holds
   `Store<AdapterHostState>`. Applies to ANY consumer with
   cross-instance sync callback flows, not just ducklink — so it
   passes the "primitives must stand independently on wasm-level
   shape" bar (per memory `feedback_wasmos_not_fiji_specific`).
2-3 wasmos-side sessions.

B. **Restructure ducklink's nested-exec** to avoid cross-instance
   reentry. Options: run nested SQL on a sibling core with
   post-hoc replay to primary (loses same-catalog visibility);
   thread the SQL back up to layer 2 as an out-of-band effect
   (async yield pattern; major reshape). Estimated 1-2 weeks
   design + implementation.

C. **Accept Path B partial.** Retire wasmtime Cargo dep and
   consumer-file wasmtime types where possible; keep
   `primary_nested_exec` + `PRIMARY_STORE_REENTRY` as a
   documented unsafe internal pattern. This is the plan doc's
   original "Alternative pragmatic scope". Estimated 6-10 days
   focused work — but the CoreExecution swap (Slice 2+3+4.3) is
   still blocked until (A) lands, because `primary_nested_exec`
   currently types on `Store<CoreStoreState>`.

**Path forward — Path (A) executed:** wasmos Phase 6.24 shipped
2026-09-22 as seven commits (design doc, primitive
implementation, tests, state-of-the-abstraction entry, follow-up
`SyncInstance::resource_drop` and `call_export_via_store`
primitives).

**Bonus finding**: cross-instance sync reentry works under the
default `RuntimeConfig` (`allow_host_callback_reentry = false`)
too. Unlike Phase 6.22's same-instance primitive, wasmtime's
`may_enter` gate on the source has no "already-entered" state to
reject the entry, because the entered instance is the CALLER's,
not the source's. Consumers using the primitive don't have to
trade away `wasm_component_model_async` / concurrency / streams
/ futures / component-model threading.

**Ducklink-side rewire (Slice 2+3+4.3, 2+3+4.5) — LANDED
2026-09-22:**

- `ad3156a` — Slice 3 atomic surgery. CoreExecution flipped to
  `sync_inst: SyncInstance`. instantiate_core rewritten via
  `SyncRuntime::from_runtime`. 5 host imports migrated to
  `HostImports::register_sync`. 16 handler downcasts flipped
  onto `<CoreInnerState>`. Escape-hatch free functions retired
  their `sync_export_bridge` dispatch, now go through
  `sync_inst.call_export("iface#method", args)`. CoreResourceHandle
  changed from newtype-around-`wasmtime::ResourceAny` to a
  two-`u64` (store_id, handle_id) struct. All 8+ resource-holding
  state types (ConnectionEntry, StreamEntry, PreparedEntry,
  AppenderEntry, SiblingSlot, DriverCoreState, `current_connection`,
  etc.) carry CoreResourceHandle. PrimaryReentry uses
  `SyncCrossInstanceHandle` + `CoreResourceHandle`.
  primary_nested_exec dispatches through
  `handle.call_export_via_store(...)`. resource_drop_handle uses
  `sync_inst.resource_drop(store_id, handle_id)`.
- `a79c6f6` — Slice 2+3+4.5 partial. CoreStoreState alias retired
  from the CoreExecution path (documented as shell-driver-only).
  Handler error-message strings tidied to name CoreInnerState.

**What remains for full Path B closure:** Phase 6 (wasmtime Cargo
dep drop) is blocked on three deeply-cascading consumer paths.
Each has been investigated in-session (2026-09-22, post-`39ac622`);
none is a single-session migration.

### Blocker 1 — `DotcmdInstance` + `compose_dynlink` linker integration

Current shape (`crates/ducklink-host/src/lib.rs:3757`):
```rust
struct DotcmdInstance {
    store: Store<DotcmdState>,
    instance: wasmtime::component::Instance,
}
```

Uses `compose_dynlink::add_to_linker::<DotcmdState>(&mut linker)`
from the wasmtime-shaped `datalink_dynlink` crate. The
wasmos-native counterpart (`datalink_dynlink_wasmos::
install_host_imports`) exists and takes a `HostImports` +
`Arc<ProviderBackend>` — but with a DIFFERENT `ProviderRegistry`
type than the wasmtime-shaped one that ducklink currently uses
process-wide.

**Concrete unblock condition (Phase 6.2.d.4, coordinated
ducklink-runtime + ducklink-host migration):**
1. Migrate ducklink's process-wide provider registry from
   `datalink_dynlink::ProviderRegistry` (constructed with
   `wasmtime::Engine`) to `datalink_dynlink_wasmos::ProviderRegistry`
   (constructed with `Arc<dyn wasmos_runtime_api::Runtime>`).
   Cascades through: `dynlink_provider_registry()` (lib.rs:3488),
   `register_env_providers()`, `sub_ext::SubExtLoader`
   (which currently uses the wasmtime `ProviderRegistry` and has
   test call-sites like `sub_ext.rs:586,633` that construct
   `Engine` directly).
2. Migrate `ExtensionStoreState` in `ducklink-runtime` off the
   `impl_compose_dynlink_host!` wasmtime-macro path onto the
   wasmos `HostImports::register` path.
3. Once both are on the wasmos path, DotcmdInstance rewrites to
   `SyncInstance` + `HostImports::register(compose:dynlink/linker,
   ResidentBackend::new(registry))` at instantiate time. The
   `store` and `instance` fields disappear behind `SyncInstance`.

**Estimated scope:** 3-5 focused sessions. Cross-crate ordering
matters — the ProviderRegistry migration is atomic (breaks all
callers simultaneously), so it lands as one coordinated commit
per ducklink-runtime + ducklink-host repo.

### Blocker 2 — `build_engine_for_driver` returns `wasmtime::Engine`

Current shape (`crates/ducklink-host/src/lib.rs:11051`):
```rust
pub fn build_engine_for_driver() -> Result<Engine> { build_engine() }
```

Callers (all inside ducklink-host — no external API break needed):
- `cron_cli.rs` — 7 sites, each `let engine = build_engine_for_driver()?;`
  then passes to `open_state(&engine, ...)` → `open_driver_core(engine, ...)`.
- `driver_exec.rs:333` — one site, threads through `DriverStoreState.engine`.

`open_driver_core_with_bootstrap()` uses the engine ONLY for
`ExtensionManager::new(engine.clone())`. `ExtensionManager`
stores + uses that engine for compiling extension `.wasm` files
(bindgen path) and for constructing per-extension components.
The `instantiate_core()` path already builds its own SyncRuntime
internally and does NOT use the passed-in engine.

**Concrete unblock condition:** migrate `ExtensionManager` off
`wasmtime::Engine` onto `Arc<SyncRuntime>` (or a wasmos-native
compile-cache factory). The bindgen extension-load pipeline uses
`wasmtime::component::Component::new(&engine, bytes)` + linker
+ store — retiring these is a large multi-file rewrite touching
extension-load, extension-instance, extension-dispatch (see
`crates/ducklink-host/src/lib.rs` around ExtensionManager +
`ExtensionInstance` from ~line 6540).

**Estimated scope:** 5-8 focused sessions. Once ExtensionManager
is wasmos-native, `build_engine_for_driver` can return
`Arc<SyncRuntime>` (or a thin `DriverRuntime` newtype) and the
callers change their local variable type only.

### Blocker 3 — `wasmtime::Cache::from_file` in `build_engine()`

Current shape (`crates/ducklink-host/src/lib.rs:11260`): configures
the shared wasmtime compile cache so ~96 MB core component
compilation amortises across CLI invocations. Retirement requires
wasmos-side `RuntimeConfig` to accept a compile-cache handle
(a future primitive; not on any active roadmap doc as of
2026-09-22).

**Concrete unblock condition:** wasmos-side addition of
`RuntimeConfig::with_compile_cache_from_file(Option<PathBuf>)` or
similar. Documented as "future wasmos gap" in `instantiate_core`
comment at lib.rs:10780+. This is legitimately outside the ducklink
team's scope until wasmos adds the primitive.

**Estimated scope:** 1-2 wasmos sessions (design + primitive +
adapter plumb-through), then a mechanical ducklink update.

### Session summary (2026-09-22, post-`39ac622`)

Investigation confirmed all three items are genuinely
multi-session blockers, not overlooked one-liners. The remaining
`wasmtime::` residue in `crates/ducklink-host/src/lib.rs`
(currently 19 refs) breaks down as:
- 2 real-code import lines (`use wasmtime::…` at :229-230) —
  live, feed Blockers 1 + 2.
- 3 real-code type/method sites — `DotcmdInstance.instance` at
  :3765 (Blocker 1), `wasmtime::Cache::from_file` at :11260
  (Blocker 3), one Linker construction at :3843 (part of
  Blocker 1's compose_dynlink surface).
- ~14 archaeological doc comments (comments describing retired
  code — intentionally preserved as migration history per
  `feedback_wasmos_foundational_correctness`).

Blocker 1 investigation confirmed `datalink-dynlink-wasmos`
(the wasmos-native ProviderBackend crate at
`~/git/datalink/crates/datalink-dynlink-wasmos/`) exists and
its `install_host_imports` API is production-shaped, but the
ducklink-side switchover (Phase 6.2.d.4) has not landed.

Blocker 2 investigation confirmed the `open_driver_core`
threading only reaches ExtensionManager — no test or external
consumer directly consumes the `wasmtime::Engine` return type,
so the switchover is scoped to ducklink-host + one type flip
per caller once ExtensionManager migrates.

Blocker 3 remains an acknowledged wasmos-side gap.

Ducklink-host's wasmtime uses now sit at **19 lines** (down from
~110 at Slice 2 start — a **-83% reduction** across the arc):
- 2 real-code `use wasmtime::…` import lines (feed Blockers 1 + 2)
- 3 real-code type/method sites (Blockers 1 + 3)
- ~14 archaeological doc comments (migration history — preserved)

CliHarness + run_cli_inner + wire_cli_bridged_host_imports +
dispatch_cli_run all landed on the wasmos-native path
(`1ac637c`, 2026-09-22). The ExtensionManager `wasmtime::Result`
interop closed in `0b0a302`. The standalone-shell driver migrated
in `4f56c37`. Cross-instance CoreExecution dispatch fixed in
`39ac622`.

The three remaining consumer arcs are independent; neither blocks
the others. Full Cargo dep drop requires all three to land plus
retiring the archaeological doc references (or updating the
lint to allow bare `wasmtime` mentions in comments).

Path Slices 1+2 (`aae7e12` + `d07469e`) remain valid prep for
whichever path is chosen.

**Total realistic scope:** 3-4 weeks focused ducklink-team work.
Phase 5 adds 1-2 weeks ecosystem lag for extension-author
coordination (semver-major `ExtensionServices` trait break in
`ducklink-runtime` cascades to downstream community extensions).

**Alternative pragmatic scope:** Phases 1-4 + 6 only. Ducklink
keeps `unsafe fn primary_nested_exec` (defensible pattern
following wasmtime's own reentry precedent) but retires the
direct wasmtime Cargo dep. ~6-10 days.

**One-session cap:** three to five substeps of Phase 1 fit a
focused single session; the full plan across all six phases is
multi-session by construction (Phase 5 alone dominates calendar).

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

**Status: RETIRED (2026-09-22).** Phase 6.24 (wasmos
`SyncCrossInstanceHandle::call_export_via_store`, wasmos commit
`daeaed79`) supersedes this phase. `primary_nested_exec` now
dispatches through the wasmos-native primitive with no
`ExtensionServices` trait break needed — `nested_exec` keeps its
`&mut self, sql` signature. See the fusion note above and Slice
3's landing (`ad3156a`) for the final shape.

The design description below is preserved for archaeology — it
was the plan before Phase 6.24 discovered the cross-instance
sync reentry primitive could work without ctx-threading. Do not
implement.

**Goal (RETIRED):** retire `unsafe fn primary_nested_exec` +
`PrimaryReentryGuard` + `PRIMARY_STORE_REENTRY` TLS. Replace with
`HostCallContext::reentry()?.call_export(...)` inside the
callback-dispatch chain.

**Cross-crate scope (RETIRED):**

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
