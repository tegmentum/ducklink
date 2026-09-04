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
5. **Sync `Runtime` facade** — a `SyncRuntime` alongside `Runtime`
   that mirrors every `async fn` as `fn` for sync consumers. Would
   remove the sync→async cascade blocker.
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
