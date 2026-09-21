# Wasmos-runtime-api migration recipe

Synthesised from two structured research passes (2026-09-04) — one
enumerating every `wasmtime::component::bindgen!` site in
`ducklink-host`, one mapping the `wasmos-runtime-api` +
`wasmos-runtime-wasmtime-v48` public surface.

Applies to both `ducklink-host` (5 bindgen sites, ~23k lines) and
`icd-9` (1 bindgen site, ~1k lines). This doc is written from the
ducklink-host perspective; the icd-9 arm inherits the same recipe.

---

## Executive summary

**The migration is bigger than "swap `use wasmtime::…` for
`use wasmos_runtime_api::…`" makes it sound.** Three properties of
the current ducklink-host code do not have a native
`wasmos-runtime-api` counterpart today:

1. **Every bindgen site is synchronous.** `wasmos-runtime-api`'s
   native surface is fully async — `#[async_trait] Runtime`,
   `async fn instantiate`, `async fn call_export`,
   `async fn HostCall::call`. Ducklink's `_sync` linker/call
   pattern must either move to async (large cascade — the whole
   handler layer, sibling-core reentry TLS, `#[test]` bodies) or
   go through the adapter's `sync_bridge` escape hatches.
2. **`ResourceTable` is a placeholder in the API today.** Site 2
   uses opaque `ResourceAny` for connection/stream/prepared/appender
   entries; site 3 uses typed `Resource<T>` for the CLI world; site
   5 uses a **native-type resource override** (WIT `connection`
   resource is bound to `crate::driver_exec::DriverConnection`
   stored directly in the wasmtime `ResourceTable`). None of these
   patterns work through `wasmos-runtime-api`'s current
   `resource::ResourceTable` (which is a `_phase_1a_placeholder(&self)`
   trait — real push/get/get_mut/delete blocked on the ducklink
   workload validation the migration is FEEDING).
3. **`wasi:http` plumbing is declarative-only.** Ducklink chose
   `add_only_http_to_linker_sync` explicitly (comment at
   `lib.rs:296-306`) to avoid a double-add clash. `wasmos-runtime-api`
   auto-wires the full `wasi:http` linker on every instance
   unconditionally; there is no consumer hook for outbound
   interception, mock responses, or per-tenant policy.

Two additional issues are secondary but real: the `with:` map on
sites 2/3/5 remaps standard WASI interfaces to specific
`wasmtime_wasi::p2::bindings::…` types (site 5 additionally maps a
resource type to a native Rust struct) — the wasmos surface has no
`with:` equivalent; and site 3 hand-rolls two host interfaces via
`linker.instance(...).func_wrap(...)` outside the generated
`add_to_linker`.

**The pragmatic wedge:** use the adapter's officially-blessed
escape-hatch modules — `sync_bridge`, `sync_bridge_resource`,
`sync_export_bridge`, `async_bridge` at
`~/git/wasmos/runtime/wasmtime/v48/src/` — which are the ONLY places
wasmtime types leak through wasmos-owned code on purpose (ADR-0029
§27 `V48_ADAPTER_ESCAPE_HATCH_FILES` allowlist). They exist
explicitly for mid-migration consumers like ducklink. Migrating
through them means:

- Direct `wasmtime::{Engine, Store, Linker, Component}` type usage
  in ducklink source stays as-is.
- Host imports and export calls go through `sync_bridge` /
  `sync_export_bridge` wrappers whose signatures still expose
  `wasmtime::component::Linker<S>` / `wasmtime::component::Instance`
  / `wasmtime::StoreContextMut<T>`.
- `wasmos_runtime_api::Value` shows up as the wire format at the
  bridge boundary, but the surrounding code keeps wasmtime types.
- Direct wasmtime deps in `Cargo.toml` remain until the escape
  hatches themselves are retired — Phase 5's "drop wasmtime from
  Cargo.toml" is a false goal on this wedge; the honest end state
  is "no `wasmtime::component::bindgen!` invocations in this crate;
  wasmtime is still a direct dep."

Going through the **native** wasmos-runtime-api surface (drop the
direct wasmtime dep, use `HostImports::register` +
`Instance::call_export`) is blocked on wasmos-side Phase 1b work
(`ResourceTable` real API), a sync→async cascade, and either a
`WasiHttpCtx` plumbing hook or a formally-documented policy that
mid-migration consumers accept declarative HTTP.

---

## Inventory — the 5 bindgen sites

| # | File / lines | World | Guest calls | Host impls | HTTP | Resources |
|---|---|---|---|---|---|---|
| 1 | `handler.rs:20-25` | `duckdb:handler/request-handler` | 1 (`call_handle`) | 0 | No | None |
| 2 | `lib.rs:1-18` (`duckdb_core_bindings`) | `duckdb:component/libduckdb` | ~21 verbs, ~46 sites | 5 (extension-loader, extension-loader-hooks, callback-dispatch, tvm/manager, tvm/bytes) | Yes | Opaque `ResourceAny` for Connection/Stream/Prepared/Appender |
| 3 | `lib.rs:20-34` (`duckdb_cli_bindings`) | `duckdb:cli/duckdb-cli` | 1 (`wasi_cli_run().call_run`) | 5 + 2 raw `func_wrap` (`host-extension-loader/request-load`, `dotcmd-host/{invoke,list-commands}`) | Yes | Typed `Resource<cli_db::{Connection,ResultStream,PreparedStatement,Appender}>` |
| 4 | `lib.rs:36-42` (`dotcmd_bindings`) | `duckdb:dotcmd/dotcmd` | 2 (`call_list_commands`, `call_invoke`) | 1 (`dotcmd/spi`) | Yes | None in world; store carries `ResourceTable` for WASI |
| 5 | `lib.rs:60-77` (`driver_tool_bindings`) | `duckdb:driver-tool/cron-driver-tool` | 1 (`wasi_cli_run().call_run`) | 2 (`driver/exec` + `HostConnection`) | No | `Resource<DriverConnection>` — **native-type override** |

All sites are SYNC. All four `lib.rs` sites set
`require_store_data_send: true` and provide a custom
`impl wasmtime::component::HasData` with
`type Data<'a> = &'a mut Self`.

### Non-bindgen wasmtime footprint

Beyond the bindgen sites, ducklink-host also has:

- **Primary-store reentry TLS** (`lib.rs:994-1027`, `unsafe fn primary_nested_exec` at `lib.rs:7758-7772`) — stashes raw `*mut Store<CoreStoreState>` + `*const duckdb_core_bindings::Libduckdb` in a TLS to re-enter the primary store from a callback. **No wasmos-runtime-api equivalent.** Would need to be reworked or kept behind the escape hatch.
- **Sibling-core replay archive** (`lib.rs:783-878`) — Phase-4 shared-`ExtensionManager` pattern.
- **`at5_intercept.rs`** (1,722 lines) — SQL-level extension interceptor. No bindgen inside it (per the grep), but it's likely deep in wasmtime.
- **TVM slot-generation table** (`lib.rs:327, 608-645`) — hand-rolled generational-index scheme sitting OUTSIDE the wasmtime `ResourceTable`. This one is *already* wasmtime-independent and moves cleanly.

---

## The recipe — two paths

### Path A: adapter escape hatch (RECOMMENDED for ducklink-host today)

**When to pick:** consumer needs to preserve sync semantics, needs
custom `WasiHttpCtx` plumbing, uses `ResourceAny`, uses native-type
resource overrides, or has hand-rolled `func_wrap`ed interfaces.
Ducklink-host is 5-for-5.

**Retained wasmtime types in consumer code:**
`wasmtime::{Engine, Store, StoreContextMut, component::{Linker, Component, Instance}}`.

**Removed wasmtime pattern:**
`wasmtime::component::bindgen!` invocations. Generated typed
`Host` traits and typed `bindings.foo_bar_baz().call_xxx()`
dispatchers are gone.

**Host-import migration (per interface):**

Old (bindgen-generated):
```rust
impl duckdb_core_bindings::duckdb::extension::callback_dispatch::Host for CoreStoreState {
    fn call_scalar(&mut self, id: u32, args: Vec<Value>) -> Result<Vec<u8>> {
        // ...
    }
}
// then:
duckdb_core_bindings::duckdb::extension::callback_dispatch::add_to_linker(
    &mut linker, |s: &mut CoreStoreState| s,
)?;
```

New (through `sync_bridge_resource::install_host_call`):
```rust
use wasmos_runtime_api::{HostCall, HostCallContext, RuntimeResult, Value};
use wasmos_runtime_wasmtime_v48::sync_bridge_resource;

struct CallbackDispatchHost;

// Implement the sync trait — one method dispatches by kebab-cased name.
impl wasmos_runtime_api::SyncHostCall for CallbackDispatchHost {
    fn call(&self, ctx: &mut HostCallContext<'_>, method: &str, args: Vec<Value>)
        -> RuntimeResult<Vec<Value>>
    {
        match method {
            "call-scalar" => {
                // Unpack args positionally
                let id = match args.get(0) { Some(Value::U32(n)) => *n, _ => bail!("id") };
                let payload = match args.get(1) {
                    Some(Value::List(items)) => items.iter().map(unpack_value).collect::<Vec<_>>(),
                    _ => bail!("args"),
                };
                let bytes = ctx.consumer_state::<CoreStoreState>()
                    .expect("consumer_state<CoreStoreState>")
                    .call_scalar_impl(id, payload)?;
                Ok(vec![Value::List(bytes.into_iter().map(Value::U8).collect())])
            }
            // ... call-scalar-batch-col, call-table, call-aggregate-col, ...
            other => Err(RuntimeError::msg(format!("unknown method: {other}"))),
        }
    }
}

// Install into the consumer-owned wasmtime linker at instantiate time:
sync_bridge_resource::install_host_call::<CoreStoreState>(
    &engine,
    &mut linker,
    &component,
    "duckdb:extension/callback-dispatch",
    Arc::new(CallbackDispatchHost),
)?;
```

Callback body itself (`call_scalar_impl`) is unchanged Rust —
same as the bindgen-era impl. Only the dispatch shell and
type marshalling change.

**Guest-call migration (per call site):**

Old:
```rust
let out = bindings.duckdb_component_database()
    .call_execute(store.as_context_mut(), conn, sql)?;
```

New (through `sync_export_bridge::call_export`):
```rust
let ret = sync_export_bridge::call_export(
    store.as_context_mut(),
    &instance,
    Some("duckdb:component/database"),
    "execute",
    &[Value::Resource { store_id, handle_id: conn_handle }, Value::String(sql.into())],
)?;
// Destructure result: WIT return was `result<execute-result, string>`
let outcome = match ret.as_slice() {
    [Value::Result { .. }] => { /* unpack */ }
    _ => bail!("unexpected shape"),
};
```

The **~46 `call_XXX` sites in site 2 alone** each need this pack/unpack
shell. Realistic mitigation: write small helper functions per
interface — `core_db::execute(store, instance, conn, sql) -> Result<...>`
— so call sites read almost the same as before, and the pack/unpack
lives in ONE place per verb. Estimated: ~150 lines of helper module
per interface, times 3 interfaces (database, extension/config,
extension/logging) ≈ 450 lines of thin marshalling glue for site 2.

**Resource handling under the escape hatch:**

Direct: keep using `ResourceAny` / `Resource<T>` in the surrounding
code. At the bridge boundary, resources become
`Value::Resource { store_id, handle_id }`. Site 5's native-type
override (`DriverConnection` stored in `store.data_mut().table`)
keeps its full current shape — the `store.data_mut().table.push(conn)`
returns a `Resource<DriverConnection>` whose raw `.rep()` is what
you'd carry across the bridge as `handle_id` (validated by the
bridge's marshaller).

**WASI + WASI-HTTP under the escape hatch:**

Keep calling `wasmtime_wasi::p2::add_to_linker_sync(&mut linker)` +
`wasmtime_wasi_http::p2::add_only_http_to_linker_sync(&mut linker)`
verbatim in the consumer code. The escape hatch does not touch
WASI wiring. `WasiCtxBuilder`, `WasiHttpCtx`, `WasiView`,
`WasiHttpView` impls all stay. This is the escape-hatch bargain:
you keep wasmtime-shaped WASI in exchange for not having to solve
the declarative-only problem.

**Site-by-site game plan:**

| Site | Path A cost | Notes |
|---|---|---|
| 1 (`handler.rs`) | Small — 1 guest call, 0 host impls, no HTTP. Cleanest first target. | Rewrite `HandlerRegistry::invoke` at `handler.rs:100-104` through `sync_export_bridge`. |
| 5 (`driver_exec.rs`) | Small-medium — 1 guest call, 2 host impls, native-type resource. | The `with:` native-type override becomes explicit — the bridge marshaller sees `Value::Resource { handle_id: rep }`, the impl looks up in `table.get_mut(&rep)`. Same table storage. |
| 4 (`dotcmd`) | Small-medium — 2 guest calls, 1 host impl, WASI HTTP. | The `Dotcmd::instantiate` non-`Pre` path becomes `component.instantiate(store, &linker)` (no bindings type). |
| 3 (`cli`) | Large — 5 host impls, 2 raw `func_wrap`, WASI HTTP, typed `Resource<T>` everywhere. | The raw `func_wrap`s convert to bridge host-installs; typed resources become bridge `Value::Resource` marshalling. |
| 2 (`core`) | Largest — ~46 guest call-sites, 5 host impls, WASI HTTP, opaque `ResourceAny`, primary-reentry TLS. | Do LAST. Split into per-interface helper modules to keep call-site diff small. Primary-reentry TLS keeps its raw pointers — the bridge tolerates it. |

**Recommended order:** 1 → 5 → 4 → 3 → 2. Each site lands as its own
commit; the tree stays green throughout.

### Path B: native wasmos-runtime-api (NOT recommended for ducklink today)

**When to pick:** consumer is greenfield or async-native, uses no
`ResourceTable` beyond WASI's, does not need `WasiHttpCtx`
plumbing, does not use bindgen `with:` overrides. `icd-9`'s
`wit_host.rs` is closer to fitting this — mid-500-lines, single
`bindgen!`, sync-flavoured but small enough that the sync→async
cascade is bounded.

**End state:** no direct `use wasmtime::…` in the consumer crate;
direct wasmtime dep can be dropped from `Cargo.toml`; the
compiled binary picks the adapter via `wasmos-runtime-select`
features.

**Blockers for ducklink specifically:**

1. `resource::ResourceTable` is a `_phase_1a_placeholder(&self)`
   trait today. Ducklink's 4 sites that carry resources need the
   real push/get/get_mut/delete surface before this path even
   compiles.
2. Sync→async cascade. Every `#[test]` body, every `handle_request`,
   every dot-command dispatcher becomes async — dozens of
   `tokio::runtime::Runtime::new()?.block_on(...)` shims OR a
   full async conversion.
3. `WasiHttpCtx` — no plumbing hook. The escape-hatch bridge is
   the only current lever.
4. `with:` native-type resource override (site 5) — no equivalent.
   `HostResourceType` marker + `new_typed_resource` gives you the
   interface+name binding but not the "store this Rust struct
   directly in the ResourceTable" storage shape.

Do not attempt Path B for ducklink until those gaps close on the
wasmos side.

---

## Known gaps to raise upstream (with wasmos)

Escalate these to `~/git/wasmos` maintainers if the goal is a
Path-B end state for ducklink:

1. **`ResourceTable` real API** — push/get/get_mut/delete/push_child
   (currently `TODO(phase-1b, workload=ducklink)` at
   `runtime/api/src/resource.rs:134-139`).
2. **`WasiHttpCtx` plumbing hook** — a consumer-side hook for
   outbound interception / mocking / per-tenant policy. Today
   `AdapterHostState` constructs `WasiHttpCtx::new()` unconditionally
   (`.../v48/src/host_state.rs:47-54`).
3. **Native-type resource storage** — the `with:` bindgen feature
   that binds a WIT resource to a native Rust type stored in the
   host's own `ResourceTable`. `HostResourceType` covers the
   marker side but not the storage side.
4. **Fine-grained WASI opt-in** — cli-only / no-filesystem / etc.
   Ducklink's `handler.rs` and `driver_exec.rs` don't need
   `wasi:http` and would benefit from being able to say so.
5. ~~**Sync `Runtime` facade**~~ ✅ LANDED (2026-09-17). Ships at
   `wasmos_runtime_wasmtime_v48::{SyncRuntime, SyncInstance}`
   (wasmos commit `a98bc76b`) behind the `sync-facade` Cargo
   feature. Owns a private tokio runtime and block_ons each async
   `Runtime` / `Instance` method. Retires the sync→async cascade
   blocker for mid-migration consumers.
6. **`add_only_http_to_linker` equivalent** — the wasmos analog of
   ducklink's chosen "avoid the double-add clash" pattern.

---

## Recommended execution sequence

1. **Phase 1 ✓ landed** (`b5c8783`) — switch wasmos deps to local
   path.
2. **Phase 2a** — rewrite `handler.rs` (site 1) as first Path-A
   proof-of-concept. Smallest surface, no resources, no HTTP, no
   host imports. ~1 commit, ~1 day.
3. **Phase 2b** — rewrite `driver_exec.rs` (site 5). Tests the
   native-type-resource story under the escape hatch. ~1 commit.
4. **Phase 2c** — rewrite `dotcmd_bindings` (site 4). Tests the
   `Dotcmd::instantiate` non-`Pre` path + WASI HTTP under bridge.
   ~1 commit.
5. **Phase 2d** — rewrite `duckdb_cli_bindings` (site 3). Largest
   host-imports surface, converts the two raw `func_wrap`s.
   ~2-3 commits.
6. **Phase 2e** — rewrite `duckdb_core_bindings` (site 2). Split
   per-interface into helper modules first, then convert call
   sites. ~5-8 commits over ~1 week.
7. **Phase 3–5** — SKIPPED under Path A. Direct wasmtime deps stay.
   Revisit if/when the wasmos gaps close.
8. **Phase 6** — `icd-9`. Single bindgen site, small surface. Path
   A recipe applies directly; Path B feasibility revisited after
   ducklink lands.

**Realistic total:** ~2 weeks focused work for the ducklink arm on
Path A, plus a small amount for icd-9. Every commit is scoped and
green. The tree is production-usable throughout — no long-lived
broken branch.

---

## Current state snapshot (2026-09-17)

Progress since the recipe was written on 2026-09-04:

- **Phase 1 ✓** (`b5c8783`, `b23155be`) — wasmos deps switched to
  local path; recipe committed.
- **Phase 2a ✓** — `handler.rs` migrated (`87caa177`).
- **Phase 2b ✓** — `driver_exec.rs` migrated (`8019b872`,
  `f233c783`).
- **Phase 2c ✓** — `dotcmd_bindings` retired (`8e66b826`).
- **Phase 2d ✓** — `duckdb_cli_bindings` retired (`816ff9bb`).
- **Phase 2e ✓ PATH A COMPLETE (2026-09-17, `e9019bd`)** — the
  last `wasmtime::component::bindgen!` block in ducklink-host has
  been retired. Wedges #1-#9 landed. What this closed:
  - Every consumer-side `bindgen!` invocation.
  - Every generated `Host` trait impl + typed
    `bindings.foo_bar().call_xxx()` accessor.
  - Every guest-export dispatch routes through
    `sync_export_bridge::call_export_with_resources`.
  - Every host-import wires through
    `sync_bridge_resource::install_host_call`.

  What this did NOT close (per Path A's honest end state, §Executive summary):
  - Direct `use wasmtime::{Engine, Store, StoreContextMut,
    component::{Linker, Component, Instance}}` in consumer code.
  - `wasmtime` + `wasmtime-wasi` in `Cargo.toml`.
  - Every `sync_export_bridge::call_export_with_resources`
    signature still exposes `wasmtime::component::Instance` +
    `wasmtime::StoreContextMut<S>` in its public API — these
    are documented escape hatches (ADR-0029 §27
    `V48_ADAPTER_ESCAPE_HATCH_FILES` allowlist).

  Path B — moving off the escape hatches to
  `wasmos_runtime_api::Instance` + `Instance::call_export`
  (async), dropping the direct wasmtime deps — is the
  outstanding work. See Path B section below (§"Path B:
  full-native migration").

  Wedges 1-6 landed:
  - #1 (`c0026776`) — `tvm:memory/bytes` host-import retired.
  - #2 (`e20b7960`) — `tvm:memory/manager` host-import retired.
  - #3 (`95b7213d`) — `host-extension-loader` host-import retired.
  - #4 (`d9859350`) — `extension-loader-hooks` host-import retired
    at the linker layer; the return-type mirror
    (`PendingRegistrationsData` → `core_extension_hooks::PendingRegistrations`
    via `convert_pending_registrations`) is deliberately kept as
    a follow-on cleanup so this wedge stays scoped.
  - #5 (`97e61270`) — **FINAL host-import** (`callback-dispatch`)
    retired. Every host-import is now bridged.
  - `c46325bc` (infra) — expose raw `Instance` on `CoreExecution`
    so guest-export wedges can reach `sync_export_bridge::call_export`
    directly.
  - #6 (`67d761a1`) — `logging` + `config` guest exports retired
    (the two simple non-resource-carrying guest interfaces).
  - `f9981f1e` (infra) — FU4 sibling/primary drain-and-archive
    protocol extracted from the retired
    `<CoreStoreState as core_extension_hooks::Host>::get_pending_registrations`
    impl into a first-class
    `CoreStoreState::drain_pending_registrations_for_replay`
    method, unblocking the lib test build (wedge #4 had left two
    test callsites reaching through the retired trait method).
  - #8 (`e008cd0f`) — `convert_pending_registrations` chain + the
    12 `bindgen_*_to_value` sub-marshallers + the
    `neutral_*_to_core` leaf lowerings + the `core_extension_hooks`
    alias itself all retired. Native `PendingRegistrationsData`
    marshals directly to `Value` via
    `native_pending_registrations_to_value`. One bindgen surface
    permanently gone. Wire form preserved verbatim.

**Wedges remaining under Phase 2e** (estimated 2 more):

1. **Guest exports: `duckdb:component/database`** (wedge #7).
   **UNBLOCKED — wasmos-side prerequisite landed 2026-09-17
   as wasmos commit `68970425`** (Option B from the earlier
   analysis: pass-through resource handle table). The
   `with_database` / `with_stream` / `with_prepared` /
   `with_appender` helpers on `CoreExecution` still hand out
   bindgen `core_db_exports::Guest{,ResultStream,PreparedStatement,Appender}`
   typed views. Call sites: ~15+ `guest.call_execute(...)`
   invocations in `HostState::execute` / ATTACH intercept /
   write intercept + a scatter of `call_close` /
   `call_register_table_function` / `call_schema` /
   `call_parameter_count` / `call_append_row` / `call_flush`
   across `HostState` and stream/prepared/appender adapters
   (~107 total invocations of `with_{database,stream,prepared,
   appender}` / `call_execute` / `call_close` /
   `call_register_table_function` / `call_schema` /
   `call_parameter_count` / `call_append_row` / `call_flush`
   at 2026-09-17). Each site converts to
   `sync_export_bridge::call_export_with_resources` with the
   interface + method name spelled out and args marshalled as
   `Value`. Estimated size: ~800-1500 line net change,
   comparable to wedge #6 but with resource handles in play —
   every appender / stream / prepared entry holds a
   `wasmtime::component::ResourceAny` that must round-trip
   through `Value::Resource` when handed back to the guest.

   **Migration shape** (available in the wasmos v48 adapter as
   of `68970425`):

   ```rust
   use wasmos_runtime_wasmtime_v48::sync_export_bridge::{
       call_export_with_resources, ExportResourceTable,
       EXPORT_TABLE_STORE_ID,
   };

   // Add to CoreExecution:
   struct CoreExecution {
       store: Store<CoreStoreState>,
       bindings: duckdb_core_bindings::Libduckdb, // retire in wedge #9
       instance: wasmtime::component::Instance,
       resources: ExportResourceTable, // NEW
   }

   // Guest-export path (returning a resource):
   let out = call_export_with_resources(
       core.store.as_context_mut(),
       &core.instance,
       Some("duckdb:component/database@5.0.0"),
       "open-appender",
       &[/* args as Value */],
       &mut core.resources,
   )?;
   // The Value::Resource inside the Ok arm holds
   // (store_id: EXPORT_TABLE_STORE_ID, handle_id: <fresh>).

   // AppenderEntry stores handle_id (u64) instead of
   // ResourceAny; append-row hands it back:
   let _ = call_export_with_resources(
       core.store.as_context_mut(),
       &core.instance,
       Some("duckdb:component/database/appender@5.0.0"),
       "append-row",
       &[
           Value::Resource {
               store_id: EXPORT_TABLE_STORE_ID,
               handle_id: entry.handle_id,
           },
           /* values as Value::List */
       ],
       &mut core.resources,
   )?;
   ```

   The table `take`s ownership on `own<T>` args and `get`s a
   keep-alive on `borrow<T>` args — dispatch is param-type
   directed via `Func::ty().params()` introspection. See
   `wasmos/runtime/wasmtime/v48/src/sync_export_bridge.rs`
   `call_export_with_resources` for the full contract +
   `ExportResourceTable` for the handle-side API.

   Wedge #7 is a mechanical mapping of the ~107 call sites
   onto that pattern, plus:
   - Adding `resources: ExportResourceTable` to
     `CoreExecution`.
   - Changing `AppenderEntry`, `StreamEntry`, `PreparedEntry`,
     `ConnectionEntry` from `handle: ResourceAny` to
     `handle_id: u64` (or a small newtype wrapping the pair).
   - Marshalling `core_types::Duckvalue` args to
     `Value::List(Value::Variant(...))` shapes at each call
     site, and unpacking returns symmetrically. The existing
     `convert_core_duckvalue` / `convert_cli_duckvalue` chains
     stay usable — they don't touch resources — but adding
     `native_duckvalue_to_value` / `value_to_native_duckvalue`
     helpers alongside them (matching the wedge #8 marshaller
     style) keeps each call site to 3-5 lines of Value
     construction.
   - Retiring `with_database` / `with_stream` / `with_prepared` /
     `with_appender` accessors once every site has moved.
2. **Wedge #7 landed 2026-09-17** across 9 slices
   (`0a620f0a`, `9e02f8e5`, `d198f320`, `0cc52386`, `b5deaac4`,
   `90d79a1f`, `f8ef8230`, `ac506ee5`). Every `with_database` /
   `with_stream` / `with_prepared` / `with_appender` callsite
   in ducklink-host is off bindgen — production, tests, and
   cross-file consumers (`dotcmd_wasmos`, `replicate`,
   `quack_server`, `ui_server`, `httpd`) all migrated. The
   four accessors + the `bindings: Libduckdb` field on
   `CoreExecution` are DELETED; `PrimaryReentry` carries
   `*const wasmtime::component::Instance` instead of
   `*const Libduckdb`. Alias count trimmed 6 -> 5 at wedge
   entry, then 5 -> 4 in wedge #9-a. See below.

3. **Wedge #9-a landed 2026-09-17** (`e9517b3`) — retired
   the `core_db_exports` alias entirely. The dead-code
   cluster (`spi_render_rows`, `convert_core_query_result`,
   `convert_core_row`, `convert_core_columndef`,
   `convert_cli_columndescriptor_to_core`,
   `convert_cli_logicaltype_to_core`,
   `convert_core_extension_info`,
   `query_result_to_nested_exec`, `extract_rows_affected`)
   is gone; `intercept_attach`'s at5 `TableShape.columns`
   flipped from `Vec<core_db_exports::ColumnDescriptor>` to
   `Vec<cli_native::ColumnDescriptor>` via a new
   `convert_extension_logicaltype_to_cli`. Bindgen aliases
   retired: 3 of 6 total (`core_extension_hooks` wedge #8,
   `core_runtime_exports` zero-caller cleanup,
   `core_db_exports` wedge #9-a).

4. **Wedge #9-a follow-up (2026-09-17, commit `1ef699b`)** —
   dead-code sweep of 14 conversion helpers + 2 subject-of-
   retired-function tests. `core_types` reference count
   halved from 359 -> 194 (46% reduction).

5. **Wedge #9-b/-c/-d/-e landed 2026-09-17** (`e9019bd`) —
   the final and largest wedge. Every remaining `core_*`
   alias was redirected from bindgen output to a hand-written
   runtime mirror of the same WIT shape:

   - `use core_types = ducklink_runtime::extension` — 194
     references (`Duckvalue` / `Duckerror` / `Logicaltype` /
     the 6 payload leaves + `Capabilitykind` / `Funcflags` /
     `Decimalshape`) all resolve to the neutral runtime types
     defined by hand in
     `crates/ducklink-runtime/src/extension.rs`.
   - `use core_column_types = ducklink_runtime::extension` —
     `Colvec` / `Column` and their re-exports.
   - `use core_callback_dispatch = ducklink_runtime::extension` —
     `Invokeinfo` / `Resultset` and their re-exports.
   - `mod core_tvm_types { pub use tvm_core::{Handle,
     RegionKind, TvmError}; }` — TVM handles routed to the
     tvm-core crate's own types. `tvm_core::TvmError` has
     two additional arms (`UnsupportedAllocator`,
     `PolicyViolation`) that don't cross the WIT boundary
     today; `bindgen_tvm_error_to_value` folds them into
     `backing-store` with a diagnostic string.

   With every code reference redirected, the top-level
   `pub mod duckdb_core_bindings { wasmtime::component::bindgen!(...) }`
   block was DELETED. The `duckdb_core_bindings::*` symbol
   tree (generated trait defs, Host trait, Libduckdb wrapper,
   Libduckdb{Pre,Indices}, per-interface Guest views, type
   mirrors) has zero remaining consumers in the crate.

   Compile-time win: `cargo build -p ducklink-host` no
   longer expands the bindgen! macro. The recipe's Phase 2e
   closes with zero bindgen sites remaining in ducklink-host.

## Phase 2e status: ✅ COMPLETE

Every bindgen! site in `ducklink-host` is retired. Every
`with_*` accessor is retired. Every guest-export dispatch
routes through
`wasmos_runtime_wasmtime_v48::sync_export_bridge::call_export_with_resources`;
every host-import wires through
`wasmos_runtime_wasmtime_v48::sync_bridge_resource::install_host_call`.
`CoreExecution` holds only `store: Store<CoreStoreState>` +
`instance: wasmtime::component::Instance` — no bindgen-typed
wrapper, no Host trait implementations. Runtime behavior is
preserved (same WIT wire format via the wasmos escape hatch;
same 128 lib tests passing; same 30 pre-existing infra
failures unchanged).

**Phase 6 ✓ PATH A COMPLETE** — every sibling consumer of the
recipe is migrated OFF the `bindgen!` macro but STILL depends
on `wasmtime` + `wasmtime-wasi` directly (the shared honest end
state described in §Executive summary):

- **icd-9** (2026-09-17, `f94fd16` in `~/git/icd-9`) — the
  first Path-A sibling migration; ducklink-host's Phase 2b
  driver_exec pattern ported line-for-line.
- **icd-10** (2026-09-17, `c1fec76` in `~/git/icd-10`) — same
  Path-A migration ported from icd-9. Two additional catalog
  methods over icd-9 (`axes(code)`,
  `axis-titles(section)`) added their own `unpack_pcs_axes` +
  `unpack_pcs_axis_title` marshallers. 58 lib tests pass; the
  `no-default-features` build stays clean (wasmos deps stay
  optional). `wasmtime` + `wasmtime-wasi` bumped 47 -> 48 to
  match the wasmos v48 adapter.

Every `wasmtime::component::bindgen!` invocation across the
ducklink family (ducklink-host, icd-9, icd-10) is retired.
Direct `use wasmtime::…` in consumer code, and `wasmtime` +
`wasmtime-wasi` in `Cargo.toml`, both REMAIN.

## Path B: full-native migration (partially landed 2026-09-21)

Ends the wasmos-runtime-api migration arc's real goal — zero
direct `wasmtime` dependency in any ducklink-family consumer.
The escape-hatch bridges (`sync_export_bridge` /
`sync_bridge_resource` / `sync_bridge` / `async_bridge`)
themselves stay in wasmos-runtime-wasmtime-v48; the goal is
that no ducklink-family Cargo.toml + no ducklink-family
`.rs` file names them.

**Landed consumer migrations (2026-09-21):**

- ✅ `driver_exec.rs::run_driver_tool` — moved to
  `SyncRuntime::compile_component` + `SyncRuntime::instantiate` +
  `HostImports::register_sync` + `SyncInstance::call_wasi_command`.
  Retired `sync_bridge_resource::install_host_call` +
  `sync_export_bridge::call_export`. `DriverStoreState.engine`
  (wasmtime::Engine) remains as a legitimate ducklink-internal
  reference — `DriverConnection::open` spins up the persistent
  DuckDB core wasm and that machinery is not a Path B target
  yet. Ducklink commit `0e10007`.
- ✅ `handler.rs::HandlerRegistry` — fully retired every direct
  wasmtime type. Stores compiled components as
  `wasmos_runtime_api::CompiledComponent`; each invoke goes
  through `SyncRuntime::instantiate` +
  `SyncInstance::call_export("iface#method", args)`.
  Ducklink commit `59151d3`. Third consumer to leave the escape
  hatch (after icd-9 and icd-10, though those are wrappers over
  ducklink).

**Remaining Path B work in ducklink-host:**

The 16k-line `lib.rs` still uses wasmtime types extensively —
but those uses are the DuckDB CORE machinery (spinning up
`ducklink-core.wasm`, dispatching guest exports on it, plumbing
`ResourceAny` handles for connection resources, running the
sibling-store TLS reentry pattern). This is ducklink-host
*being a wasm host*, not ducklink-host *consuming the wasmos
API*. Retiring these would require moving the DuckDB core
itself through `SyncRuntime` — a multi-file rewrite touching
`CoreExecution`, `ExtensionManager`, the primary-reentry TLS,
and every `with_database` / `with_appender` / `with_stream` /
`with_prepared` helper.

Similarly, `quack_server.rs`, `ui_server.rs`, `replicate.rs`,
`httpd.rs`, `cron_cli.rs`, `dotcmd_wasmos.rs` all name
`wasmtime::Engine` or `wasmtime::component::ResourceAny` — but
each of those references crosses into `CoreExecution` / the
DuckDB core, not the driver/handler tool components. Same
"ducklink IS the host" story.

Estimated scope for lib.rs migration: ~2 weeks. Requires
async-safe redesign of the primary-store reentry TLS
(`unsafe fn primary_nested_exec`) before the async cascade
can compile.

**What has to change** (per §Executive summary):

1. **Every `use wasmtime::…` removed from consumer code.**
   `Store<S>` becomes wasmos's opaque store handle (per
   `wasmos_runtime_api::ExecutionContext` + consumer_state);
   `Instance` becomes `wasmos_runtime_api::Instance`;
   `Engine` becomes `wasmos_runtime_api::Runtime` (its
   `WasmtimeV48Runtime` impl is one option among many);
   `Linker` disappears (host imports register via
   `HostImports::register`).
2. **Sync -> async cascade.** `Instance::call_export` is
   `async fn`. The whole handler layer, sibling-core reentry
   TLS in `HostState::execute`, and every `#[test]` body that
   drives guest exports has to move to `async`.
3. **Resource marshalling via wasmos's `ResourceTable`.**
   ✅ **PATH B STEP LANDED (2026-09-17)** — wasmos
   `ResourceTable` Phase 1b API (`push` / `get` / `get_mut` /
   `delete`) landed at wasmos commit `e174e9e8`; all three
   consumers migrated their WORKLOAD-OWNED tracking to it
   (icd-10 `85bbf5d`, icd-9 `2f0f17d`, ducklink-host
   `b863d0c`). The pattern is split-tables: keep a
   `wasmtime::component::ResourceTable` for wasmtime-wasi's
   `WasiView`/`WasiHttpView` (canonical-ABI resource lifting,
   mandatory dep of the wasi/wasi-http traits) alongside a
   `wasmos_runtime_api::ResourceTable` for the consumer's own
   push/get/delete. This eliminates `wasmtime::component::Resource<T>`
   from consumer signatures; only the WasiView-backing
   `ResourceTable` type still touches wasmtime — that gap
   closes when (4) below lands.
4. **`wasi:http` plumbing hook.** Ducklink-host picks
   `add_only_http_to_linker_sync` explicitly to avoid a
   double-add clash; wasmos-runtime-api auto-wires the full
   `wasi:http` linker unconditionally. Path B needs either a
   consumer hook for outbound interception / mock responses /
   per-tenant policy, or a formal ADR that mid-migration
   consumers accept declarative HTTP.
5. **`with:` map equivalent.**
   ✅ **PATH B STEP VERIFIED (2026-09-17)** — bindgen's `with:`
   map decomposes into three orthogonal wasmos features that
   are ALL already shipped:
   - **Standard-WASI-interface override** (sites 2/3
     remapped `"wasi:cli/environment"` etc. to
     `wasmtime_wasi::p2::bindings::cli::environment`): under
     the escape-hatch bridge, `wasi_p2::add_to_linker_sync`
     wires up the whole WASI surface in one call — the
     interface override becomes implicit.
   - **Native-type resource storage** (site 5 mapped WIT
     `connection` → native `DriverConnection`):
     `wasmos_runtime_api::ResourceTable::push::<T>` (Phase 1b
     landing, wasmos `e174e9e8`) stores native Rust types
     under `Resource<T>` handles with type-checked
     `get`/`get_mut`/`delete`.
   - **Compile-time host-handler signature typing**
     (bindgen's `impl HostConnection for DriverStoreState`):
     `#[host_iface(sync)]` on an inherent impl block
     generates `impl SyncHostCall` doing dispatch-by-kebab-name
     plus arg-lift/return-lower for typed `Resource<T>` args
     and returns. `#[derive(HostResourceType)]` on the marker
     binds the WIT interface + name.

   Together these three cover the full `with:` shape. No new
   wasmos-side API is needed; what remains is consumer
   adoption of `#[host_iface(sync)]` in place of the current
   Vec<Value>-passthrough `SyncHostCall::call` — a
   consumer-side migration, not a wasmos-side blocker.
6. **Primary-store reentry TLS.** `unsafe fn primary_nested_exec`
   stashes `*mut Store<CoreStoreState>` +
   `*const wasmtime::component::Instance` in a TLS to
   re-enter the primary store from a callback.

   ✅ **FULLY LANDED (2026-09-21)** —
   [wasmos phase-6-22-async-safe-reentry.md](../../../wasmos/docs/design/runtime-abstraction/phase-6-22-async-safe-reentry.md).
   Design at wasmos commit `993db508`; API + adapter at
   `0097cdf2`; tests + constraint finding at `4ffbc7e3`;
   **config-gated capability at `024a3b4a`**.

   Consumers opt in via `RuntimeConfig::allow_host_callback_reentry = true`.
   Under that config, the v48 adapter configures wasmtime with
   `wasm_component_model_async(false)` + `concurrency_support(false)`
   (trading streams / futures / component-model threading for
   reentry), and advertises `WASMOS_HOST_CALLBACK_REENTRY`. Host
   handlers reach the primitive via
   `ctx.reentry()?.call_export(name, args).await`.

   Two integration tests lock the shape:
   - `host_handler_reentry_succeeds_with_allow_config`: proves
     `outer(21)` → host `trigger(21)` → reentry → guest
     `double(21)` → 42 works end-to-end under the opt-in config.
   - `host_handler_reentry_traps_under_async_component_model`:
     locks the `CannotEnterComponent` trap under the default
     async config.

   **Ducklink migration**: `primary_nested_exec` retirement is now
   unblocked. Consumer surface is:
   1. Build the runtime with `RuntimeConfig { allow_host_callback_reentry: true, .. }`
      (or via `SyncRuntime::new(...)` with that config).
   2. Replace `PrimaryReentryGuard` + `PRIMARY_STORE_REENTRY` TLS +
      `unsafe fn primary_nested_exec` with a
      `CoreStoreState.entry_connection: Option<Value>` field set
      at `open` time.
   3. In `nested_exec`, call
      `ctx.reentry()?.call_export("duckdb:component/database#execute", &[conn, sql]).await`.

   Estimated size: ~10 lines of straight-line async code replacing
   ~80 lines of `unsafe` / TLS / RAII scaffolding.

**Blocked-on-wasmos work:**

- ~~Phase 1b: real `ResourceTable` trait API in
  `wasmos_runtime_api::resource`~~ ✅ LANDED — wasmos
  `e174e9e8` (2026-09-17). Concrete `push` / `get` /
  `get_mut` / `delete` API + `ResourceError` enum + 10
  unit tests. Consumer-owned tracking validated in all
  three ducklink-family workloads.
- `wasi:http` consumer plumbing hook OR formal doc that
  declarative HTTP is the answer.
- ~~The sync→async cascade for mid-migration consumers~~
  ✅ CLOSED via `SyncRuntime`/`SyncInstance` facade
  (wasmos commit `a98bc76b`, `sync-facade` Cargo feature).
  Owns a private tokio runtime, block_ons each async
  method; consumer surface stays sync.
- ~~`Instance::call_export` variants that support the
  primary-reentry pattern under async~~ ✅ FULLY LANDED
  (2026-09-21, wasmos commits `993db508` design + `0097cdf2`
  API/adapter + `4ffbc7e3` tests + `024a3b4a` config-gated
  capability). Ducklink opts in via
  `RuntimeConfig::allow_host_callback_reentry = true`;
  `WASMOS_HOST_CALLBACK_REENTRY` advertised under that config;
  streams / futures / threading swapped out under the same
  config (workload-derived trade-off). Ducklink's
  `primary_nested_exec` migration off the sync escape hatch
  is now fully unblocked at the wasmos side.
- ~~`with:` map equivalent on `wasmos_runtime_api::HostImports`
  registration~~ ✅ VERIFIED (2026-09-17). Covered by three
  already-shipped features: `p2::add_to_linker_sync` (WASI
  interface remap), `ResourceTable` Phase 1b (native-type
  storage), and `#[host_iface(sync)]` + `#[derive(HostResourceType)]`
  (compile-time typed dispatch). Consumer adoption of the last
  pair is a separate consumer-side migration wedge.
- ~~**Wasmos-native `WasiView` hook.**~~ ✅ PARTIALLY
  LANDED (2026-09-17). wasmos ships
  `wasmos_runtime_wasmtime_v48::SyncStoreState<T>` (wasmos
  commit `bedff545`) — a public store-data wrapper that
  owns the wasmtime-wasi `WasiCtx` + wasmtime-wasi-http
  `WasiHttpCtx` + `wasmtime::component::ResourceTable`
  internally and implements `WasiView` / `WasiHttpView`
  itself, so consumer state types no longer expose those
  fields or the `impl WasiView` boilerplate. All three
  ducklink-family consumers adopted it (icd-10 `86db4dd`,
  icd-9 `856e2de`, ducklink-host `82390fb`).

  What remains: consumer surface still names
  `wasmtime::Store`, `wasmtime::Engine`,
  `wasmtime::component::{Component, Linker, Instance}` —
  those disappear only when consumers move to the wasmos-
  native `Runtime::instantiate` path. That path is now
  reachable from sync code without an async cascade via the
  `SyncRuntime` / `SyncInstance` facade (wasmos commit
  `a98bc76b`, behind the `sync-facade` Cargo feature).
  Blocker (4) proper is now a consumer-side migration
  wedge, no longer wasmos-side blocked.

**Estimated scope** (once wasmos-side prerequisites land):

- ducklink-host: substantially larger than Path A — the
  sync -> async cascade alone touches every host handler,
  every test, and every driver-core / ui-server / httpd /
  quack-server entry point. Not amenable to the "one wedge
  per WIT interface" partition Path A used.
- icd-9 + icd-10: much smaller — each has one `wit_host.rs`
  file with one `WitDriverExecHost` + 12 `call_*` helpers +
  one `dispatch()` call site tree. The mechanical shape is
  bounded by the ~150 lines of `wit_host.rs`.

The next wedge is choosing how to slice Path B given the
wasmos-side prerequisites' state — the recipe intentionally
stops short of prescribing wedges here because the wasmos
side has not committed to shapes for (3)-(5) yet, and
prescribing wedges before those shapes are known would set
consumers up to churn.

## Toolchain gotcha

The workspace's active `rustup` toolchain (1.93.0) is one minor
behind wasmtime 48's `MSRV` (1.95.0). Build with an explicit
`+1.98` (or `+nightly`) override:

```sh
cargo +1.98 check -p ducklink-host
cargo +1.98 test -p ducklink-host
```

The `~/git/wasmos` sibling checkout doesn't have this issue
because its wasmos-runtime-api path deps carry their own MSRV
that the workspace-wide 1.93.0 satisfies.
