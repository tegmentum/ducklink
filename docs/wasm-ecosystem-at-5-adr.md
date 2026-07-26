# ADR: Porting the wasm ducklink ecosystem from duckdb:extension@4 to @5

**Status:** Approved (spike-informed) — see the Amendments section at the end. Decision 4 and Decision 8 are modified from the original text below.
**Date:** 2026-07-26 (original); 2026-07-26 (spike amendment)
**Scope:** ducklink WIT + ducklink-host + ducklink-loader + duckdb-wasm/core, plus 231 workspace extensions
**Companion:** `wasm-ecosystem-at-5-spike-storage.md` — the C-API viability spike whose findings inform the amendment.

## Context

At `duckdb:extension@4.0.0`, the wasm arm's data-plane bridges are host **imports** on the DuckDB-wasm core (`~/git/duckdb-wasm/core/`):

- 8 `*-host` interfaces (`storage-host`, `index-host`, `collation-host`, `pragma-host`, `parser-host`, `optimizer-host`, `files-host`, `table-stream-host`).
- ~20 `pub extern "C"` bridge fns in `core/src/lib.rs` invoke those imports; 7 C++ files in `core/cpp/` (`wasm_storage.cpp`, `wasm_index.cpp`, `wasm_collation.cpp`, `wasm_files.cpp`, `wasm_component_optimizer.cpp`, `wasm_table_stream.cpp`, `wasm_index_optimizer.cpp`) subclass DuckDB's internal `StorageExtension` / `BoundIndex` / `OptimizerExtension` / `TableFunction` / `FileSystem` classes and call the extern bridges.
- The host (`crates/ducklink-host/src/lib.rs`, ~10.5 kLoC) implements those 8 imports (lines 492–1160) and forwards each call to the currently-loaded extension component's `*-dispatch` export via `ducklink-runtime::extension::ExtensionInstance` (`crates/ducklink-runtime/src/extension.rs`, ~5.8 kLoC).
- The loader stub (`crates/ducklink-loader/`, 273 LoC + 17 kLoC bindings) satisfies the same 8 imports as declining/empty exports so `wac plug` composed standalone components link.

At `@5.0.0` (`~/git/ducklink/wit/duckdb-extension/`, 49 files):

- All 8 `*-host` interfaces are **deleted**.
- Extension components keep exporting `storage-dispatch`, `index-dispatch`, `file-dispatch`, `storage-write-dispatch`, `index-write-dispatch`, `copy-dispatch`, `secret-dispatch`, `settings-dispatch`, `log-storage-dispatch`, `aggregate-incr-dispatch`, `arrow-ext-dispatch`, `conn-dispatch` (unchanged wire shape from @4 for most).
- Four capability groups are **deprecated at @5** and no host consumes them: `parser` / `parser-dispatch`, `optimizer` / `optimizer-dispatch`, `table-stream` / `table-stream-dispatch`. Components exporting them still load; the exports are dead code.
- Four new **host imports** for extension components appear: `query` (read-only live SELECT, best-effort), `nested-exec` (SIBLING-connection exec, autocommitted), `secret` + `secret-dispatch`, `settings` + `settings-dispatch`.

The load-bearing architectural claim of @5 is: **the wasm DuckDB core no longer participates in the storage/index/files pipeline**. The host orchestrates those flows directly against extension components; the core executes ordinary SQL over ordinary catalogs.

## Decision 1 — Target @5 core world

The wasm core drops all `*-host` imports and the 7 C++ subclass shims. The two remaining "core exports dispatch" surfaces mentioned in the brief are **rejected**: on inspection, DuckDB's `StorageExtension` / `IndexType` / `OptimizerExtension` / custom `FileSystem` mechanisms are in-process C++ subclasses; making the host drive them into the wasm-core through an export would require the core's DuckDB engine to synchronously await a foreign wasm callback while holding the query lock — the exact re-entrancy pattern the `nested-exec` sibling infrastructure was invented to avoid. Instead, storage/index/files are lifted to the host: the host resolves `ATTACH ... (TYPE sqlitewasm)` by materializing the foreign catalog through the extension's `storage-dispatch` and either (a) registering a plain-SQL VIEW / table function on the core's own catalog per attached table, or (b) using the C-API's `duckdb_create_table_function` (which IS stable) inside the core to expose a scan callback that trampolines out through `callback-dispatch`. Path (b) is what already works for extension-registered table functions today, so it is the migration path.

Concrete target world for `~/git/duckdb-wasm/core/wit/duckdb-core.wit`:

```wit
package duckdb:component;

use duckdb:extension/config@5.0.0;
use duckdb:extension/logging@5.0.0;
use duckdb:extension/runtime@5.0.0;
use duckdb:extension/types@5.0.0;
use duckdb:extension/callback-dispatch@5.0.0;
use wasi:cli/environment@0.2.6;
use wasi:cli/stdout@0.2.6;
use wasi:cli/stderr@0.2.6;
use wasi:filesystem/preopens@0.2.6;
use wasi:filesystem/types@0.2.6;
use wasi:io/streams@0.2.6;

interface database { /* unchanged from @4 */ }
interface host-extension-loader { request-load: func(name: string) -> bool; }
interface extension-loader-hooks { /* unchanged from @4 */ }

world libduckdb {
    import wasi:cli/environment@0.2.6;
    import wasi:cli/stdout@0.2.6;
    import wasi:cli/stderr@0.2.6;
    import wasi:filesystem/preopens@0.2.6;
    import wasi:filesystem/types@0.2.6;
    import wasi:io/streams@0.2.6;
    import host-extension-loader;
    import extension-loader-hooks;
    import duckdb:extension/callback-dispatch@5.0.0;
    import tvm:memory/manager@0.1.0;
    import tvm:memory/bytes@0.1.0;

    export database;
    export duckdb:extension/config@5.0.0;
    export duckdb:extension/logging@5.0.0;
    export duckdb:extension/runtime@5.0.0;
}
```

Deletions vs @4: 8 `import duckdb:extension/*-host` lines. No new imports on the core — the four new host-import interfaces (`query`, `nested-exec`, `secret`, `settings`, `secret-dispatch`, `settings-dispatch`) are for **extension components**, not the core; they are linked into per-extension linkers by the host at extension-load time, not into the core's linker.

## Decision 2 — Loader stub becomes an empty exports-only stub

Option (b): the loader becomes a no-op dispatch consumer that keeps `wac plug` composition alive but stops declaring the deleted `*-host` exports.

Rationale: standalone extension smoke tests still exercise the `callback-dispatch` and `host-extension-loader` / `extension-loader-hooks` composition surface, which do not go away. The 8 `*-host` exports on the current stub become uncompilable at @5 (their target interfaces don't exist), so they must be removed. Option (c) — a functioning in-memory dispatch consumer — buys nothing: standalone builds have no extension components to route to, so a real dispatch would still return declining/empty. Option (a) — delete the loader entirely — breaks the fieldbook-browser bypass path (`web/fieldbook`) and the standalone `.cwasm` recipe in `Makefile`.

Target `crates/ducklink-loader/wit/loader-stub.wit`:

```wit
package duckdb:loader-stub;

world loader-stub {
    export duckdb:component/host-extension-loader;
    export duckdb:component/extension-loader-hooks;
    export duckdb:extension/callback-dispatch@5.0.0;
    export tvm:memory/manager@0.1.0;
    export tvm:memory/bytes@0.1.0;
}
```

The `src/lib.rs` loses 8 trait impl blocks (`storage_host::Guest`, `index_host::Guest`, etc.) — approximately 130 LoC net deletion. `src/bindings.rs` regenerates from ~17.2 kLoC down to an estimated ~9 kLoC.

## Decision 3 — Host reshape

The host stops implementing 8 `*-host::Host` traits (~700 LoC deletion in `ducklink-host/src/lib.rs` lines 492–1160) and stops adding them to the core linker (lines 6574–6607, ~35 LoC). It gains a **query-planning shim** that intercepts SQL touching storage-registered extensions before handing to the core.

Concretely:
- Delete `use duckdb_core_bindings::duckdb::extension::{storage_host, index_host, collation_host, pragma_host, parser_host, optimizer_host, files_host, table_stream_host}` (lines 92–101).
- Delete the 8 `impl core_*_host::Host for CoreStoreState` blocks.
- Delete 8 `core_*_host::add_to_linker(...)` calls in `instantiate_core`.
- Delete the `convert_core_compare_op_to_storage`-style translators (lines 7560–7625) and the table-stream ts-filter translator (line 5725).
- Add a `resolve_attach_type(dsn, type)` intercept in `HostState::execute` (line ~4972) that: (a) looks up the storage-capable extension by TYPE name in `ExtensionManager`; (b) calls `ExtensionInstance::storage_attach` (`ducklink-runtime` line 4098, ALREADY implemented); (c) enumerates tables via `storage_list_tables` / `storage_table_columns`; (d) registers one table function per foreign table in the core via `duckdb_create_table_function` (already the mechanism for scalar/table extension registration through `callback-dispatch`); (e) each table function's `bind` reads a global (extension-id, catalog-handle, table-name) tuple, and its `scan` walks `ExtensionInstance::storage_scan_open` / `_next` / `_close` (already implemented).
- Extend the extension-side linker (`crates/ducklink-runtime/src/extension.rs` `add_to_linker` region ~line 2226) with the four new host imports: `query::add_to_linker` (backed by `HostState::execute` on a fresh connection to the same DB), `nested-exec::add_to_linker` (backed by existing `PrimaryReentryGuard` / `SiblingState` — plumbing already exists at lines 1262–1470), `secret-dispatch` and `settings-dispatch` are exports on the extension, so add a **secret registry** and **settings registry** the host consults instead of new linker entries.

LoC delta estimate: −900 (deletions) / +450 (query shim + secret/settings registries + new nested-exec/query wiring lines). Net −450 LoC in `ducklink-host`.

## Decision 4 — duckdb-wasm/core Rust+C++ rewrite

The core is dramatically simplified. **Delete all 7 C++ TUs and their headers** — 3763 LoC:

| File | Fate | LoC |
|------|------|-----|
| `cpp/wasm_storage.cpp` | DELETE (storage moves to host) | −1497 |
| `cpp/wasm_storage_bridge.h` | DELETE | −189 |
| `cpp/wasm_index.cpp` | DELETE (index moves to host) | −424 |
| `cpp/wasm_index_optimizer.cpp` | DELETE (optimizer deprecated) | −288 |
| `cpp/wasm_index.hpp` / `wasm_index_bridge.h` | DELETE | −177 |
| `cpp/wasm_collation.cpp` | DELETE (host synthesizes `CREATE COLLATION` via SQL) | −100 |
| `cpp/wasm_component_optimizer.cpp` | DELETE (optimizer deprecated) | −157 |
| `cpp/wasm_optimizer_bridge.h` | DELETE | −26 |
| `cpp/wasm_files.cpp` | DELETE (httpfs moves to host) | −211 |
| `cpp/wasm_files_bridge.h` | DELETE | −45 |
| `cpp/wasm_table_stream.cpp` | DELETE (table-stream deprecated) | −557 |
| `cpp/wasm_table_stream_bridge.h` | DELETE | −92 |

`core/build.rs` loses 7 `build_wasm_cpp(...)` calls (~24 LoC). The larger stack-size / `--allow-multiple-definition` / fs-shims machinery stays intact.

`core/src/lib.rs` (~9.9 kLoC today): delete every `bindings::duckdb::extension::*_host` use and every `extern "C"` bridge:

| Bridge fn cluster | LoC deleted |
|---|---|
| `storage_*` (attach/list/tables/columns/scan-open/next/close, format-error) | ~600 |
| `storage_write_*` (begin/commit/rollback/create-table/insert/update/delete) | ~450 |
| `index_*` (create/append/build/drop/search + format-error + `wasm_register_index_type`) | ~350 |
| `collation_*` (register + list-pull in load path) | ~120 |
| `table_stream_*` (open/next/close + ts-filter translator) | ~450 |
| `file_*` (open/read/close + format-error + `wasm_register_file_system`) | ~200 |
| optimizer + parser + pragma pull-paths | ~250 |
| Static `LazyLock` maps for `STORAGE_REGISTERED_TYPES`, `COLLATION_REGISTERED_NAMES`, `FILTERABLE_TABLES_REGISTERED`, `DECLARED_PRAGMAS`, `DECLARED_PARSERS`, `REPLACEMENT_SCANS` | ~100 |

**Total core LoC deletion**: ~2500 Rust + 3763 C++ ≈ **6.2 kLoC removed**. Additions: 0.

`core/src/bindings.rs` (~34 kLoC): regenerates via `cargo component build` after the world change. Expected size ~24 kLoC (drops the ~10 kLoC generated for the 8 `*-host` interfaces).

**The DuckDB C++ subclass registration pattern does not survive.** The only surviving DuckDB-C++-side mechanism is table function registration through the stable C API (`duckdb_create_table_function`), which is what `callback-dispatch` already routes for extension-declared tables. Custom storage / index / collation / filesystem all migrate to host orchestration.

## Decision 5 — Extension re-plug order

Grep of `extensions/*/src/lib.rs` for direct use of `*_dispatch` / `register-storage` / `register-index-type` / `register-collation` / `register-file-provider` / `register-filterable-table` (231 extensions total):

- **(a) Trivial — no host-shape use (216 extensions).** All the "pure algorithm" scalars/aggregates/tables (`aba`, `ascii85`, `base58check`, `bloom`, `chrono`, `crypto`, `emoji`, `faker`, …). Migration surface: none. Bump `duckdb:extension` dep to `@5.0.0` and regenerate `bindings.rs`; the `runtime` / `types` / `callback-dispatch` / `catalog` / `config` / `logging` interfaces are wire-identical between @4 and @5.
- **(b) Medium — declared but light use (7).** `autocomplete-component` (uses `query`), `icufns-component` (registers collation via `collation.register-collation`), `qopt-component` (optimizer — **deprecated at @5, becomes no-op**), `dplyr-component`, `prql_parser-component`, `ggsql-component` (parsers — **deprecated at @5, becomes no-op**), `webfs-component` / `s3fs-component` / `azfs-component` (register-files-provider → `file-dispatch`). Migration surface per extension: rebind + verify the host still drives the surviving dispatch export. LoC change per extension: <50.
- **(c) Heavy — non-trivial rewrite (8).** `sqlitewasm-component`, `mysqlwasm-component`, `postgreswasm-component`, `unityscan-component` (all four export `storage-dispatch` + `storage-write-dispatch`); `hnswfns-component`, `rtreefns-component`, `mobilitydb_temporal_core-component`, `postgis_core-component`, `numstream-component`, `timescale_time_bucket-component`, `cron-driver-tool` (export `index-dispatch` and/or `storage-dispatch`, some with vendored `deps/duckdb/`). Migration surface: **the dispatch exports themselves are wire-identical between @4 and @5** — the reshape is entirely on the CONSUMER side (host, not extension). What breaks: `mobilitydb_temporal_core`, `postgis_core`, `timescale_time_bucket`, `cron-driver-tool` all vendor `deps/duckdb-extension/*-host.wit` and `deps/duckdb/*-host.wit` — those files must be deleted from each extension's vendored deps to compile against @5. `fieldbook-loader/wit/deps/duckdb-extension/*-host.wit` also vendored. LoC change per extension: ~200 (delete vendored `*-host.wit` files + regenerate bindings).

**Extensions that fully break at @5 and need actual code changes: 0** (all dispatch shapes survive). Extensions that need dead-code cleanup: **8 heavy**. Extensions with silently no-op'd features (parser/optimizer/table-stream): **6 medium — 3 of which will need re-designed capability approaches for full functionality**.

## Decision 6 — Blocked items unblocked by completion

- **`fieldbook _install_sql`-pattern (Direction 1 nested-exec fallback).** Fieldbook and mosaic currently install SQL bootstrap on first call from inside a scalar (`fieldbook-component/src/lib.rs:214`, `mosaic-component/src/lib.rs:255`) because `nested-exec` at @4 works but is fragile under primary re-entry. **Unblocked** because `nested-exec` at @5 is a first-class host import with a defined sibling-connection contract (`nested-exec.wit`). **Extra design needed:** the sibling-core in `ducklink-host` (`SiblingState`, line 1262) currently has NO extensions loaded — extension-touching SQL run through nested-exec fails with the well-known "sibling core does not have host extensions loaded" error (line 1353). The fix is to plumb the primary's `ExtensionManager` into the sibling's `Store` at sibling-init time, sharing the loaded-component set. That is a ~200 LoC add to `ducklink-host` and belongs in **Phase 4** below.
- **`mosaic_create` disabled to avoid re-entry trap.** The scalar is dispatched inside the core mutex; calling back into SQL to insert into `mosaic.routes` deadlocks. **Unblocked** because at @5, `mosaic_create` can call `nested-exec` (sibling connection, no core-mutex contention). Requires the same sibling-extension-loading fix above.
- **Browser Fieldbook bypassing `fieldbook.wasm`.** `web/fieldbook/src/db.js:10` runs SQL directly against `ducklink_core.wasm`, skipping `fieldbook.wasm` because the composed browser-standalone can't load extension components at runtime (loader-stub declines). **Partially unblocked** by @5: with the reshaped loader-stub, a `wac plug`-composed `ducklink_core + fieldbook_loader + fieldbook.wasm` bundle becomes viable if the composed loader satisfies fieldbook's `nested-exec` import. **Extra design needed:** the browser loader-stub must gain a `nested-exec` implementation that either (a) opens a second in-browser core instance (bloats the bundle) or (b) serializes back through the caller's connection (violates the "no primary re-entry" invariant that motivated nested-exec). Recommend (a) as an opt-in feature flag; the Phase 1 browser-bypass stays the default.

## Decision 7 — Sequenced implementation plan

Four moving pieces hard-depend on each other's world shape. Order that keeps every intermediate state testable:

**Phase 1 — Parallel prep (1 agent-week each, 3 agents parallel).**
- 1a. Land the @5 core world in `duckdb-wasm/core/wit/duckdb-core.wit` (deletes 8 imports). Regenerate `bindings.rs`. Delete Rust extern-C bridges and C++ TUs. Core builds as a `libduckdb` component that does plain SQL only. Test: `cargo build -p duckdb-component-core --target wasm32-wasip2` + a smoke `SELECT 1` through `wasmtime`.
- 1b. In parallel: bump all 231 extensions' `duckdb:extension` WIT dep to `@5.0.0`. Wire-identical for 216 (a); vendored-dep cleanup for 8 (c); dispatch-export rebind for 7 (b). Test: `make bundle-check`.
- 1c. In parallel: reshape `ducklink-loader/wit/loader-stub.wit` and `src/lib.rs` (delete 8 `*-host` exports). Test: `wac plug` composition of core + loader + one extension still produces a runnable `.wasm`.

**Phase 2 — Host reshape (1 agent-week, blocks on 1a).**
- Delete 8 `*-host` trait impls in `ducklink-host/src/lib.rs`.
- Delete 8 linker registrations in `instantiate_core`.
- Add the storage/index/files query-planning shim in `HostState::execute`.
- Existing `ExtensionInstance::storage_scan_open/next/close/attach/list_tables/table_columns` already exists in `ducklink-runtime/src/extension.rs` — no new dispatch driver code needed; only new **shim code in the host** that CHOOSES to call it. Test: `sqlitewasm` ATTACH + SELECT roundtrip.

**Phase 3 — New host imports (0.5 agent-week, blocks on 2).**
- Wire `query`, `nested-exec`, `secret-dispatch`, `settings-dispatch` into the extension-side linker (`ducklink-runtime::extension::add_to_linker`).
- `query` and `nested-exec` already have host-side implementations (autocomplete extension already imports `query`; fieldbook + mosaic already import `nested-exec`). No new plumbing beyond the linker wire-up.

**Phase 4 — Sibling-extension loading (0.5 agent-week, blocks on 3).**
- Plumb `ExtensionManager` into `SiblingState` so nested-exec SQL can call extension functions. Unblocks the `_install_sql` workaround.

**Phase 5 — Optimizer / parser / table-stream re-homing (deferred, 1 agent-week).**
- The 6 medium-tier extensions (autocomplete uses query, icufns uses collation; qopt/dplyr/prql/ggsql use deprecated interfaces) that lose parser/optimizer/table-stream functionality get replaced with the C-API equivalents where possible, or are marked "advanced tier removed" in extension READMEs.

Total: ~4 agent-weeks; Phases 1–3 can compress to 2 weeks with 3 agents.

## Decision 8 — Risks and open questions

1. **HIGHEST: Does DuckDB's C++ storage-extension mechanism accept a stable-C-API-only substitute?** The plan assumes ATTACH-driven foreign catalogs can be entirely replaced by table functions registered per foreign table through `duckdb_create_table_function`. DuckDB's `ATTACH ... (TYPE <name>)` syntax REQUIRES a `StorageExtension` subclass registered in C++. If we cannot intercept `ATTACH` at the SQL text level cleanly (compound statements, multi-statement scripts, parameterized DSNs), the entire storage-migration plan breaks and we must keep at least a MINIMAL `storage-host` import surviving on the core. **Mitigation:** land Phase 1a behind a feature flag; keep the C++ shim available as a fallback until Phase 2 storage smoke tests pass end-to-end.
2. **`duckdb_create_table_function` filter/projection pushdown parity.** The current `wasm_storage.cpp` scan honors `wants_rowid`, projection, and filter pushdown into the scan request (`storage-host.scan-request`). The C-API `duckdb_create_table_function` supports projection and filter pushdown, but the pushdown types are more restrictive than the C++ internal `TableFilterSet`. Some `sqlitewasm` predicates may no longer push through — measurable perf regression on filtered scans.
3. **`REPLACEMENT_SCANS` for `FROM 'file.ext'` autoregistration.** The C API's `duckdb_add_replacement_scan` IS stable, so this migrates cleanly, but callers currently pass a resolved table-function name; validate that the C-API replacement-scan callback can synthesize a function reference to a runtime-registered table function.
4. **Custom index without C++ subclass.** DuckDB's index type registration (`IndexType`) has NO stable-C-API equivalent as of DuckDB 1.5.x. `hnswfns` / `rtreefns` / mobilitydb custom indexes have no @5-plan home. **Recommendation:** treat custom indexes as a KNOWN GAP at @5 and reroute those extensions to expose their scan as a table function (`hnsw_search(...)`) — which they already do today for the query path. The `CREATE INDEX ... USING <type>` path becomes a no-op; users invoke the table function directly.
5. **`nested-exec` sibling with extensions loaded.** Sharing `ExtensionManager` across primary and sibling Stores means shared `ExtensionInstance` state — including callback registries. Wasmtime stores are single-threaded, so re-entry via the sibling could still hit registry mutexes. Needs a design pass in Phase 4.
6. **`WasmStorageExtension` scan `wants_rowid` for UPDATE/DELETE.** The C-API table-function pushdown does not carry the concept of a virtual rowid column the way `WasmScanInitGlobal` currently does (`wasm_storage.cpp` detects `COLUMN_IDENTIFIER_ROW_ID`). UPDATE/DELETE on sqlitewasm-attached tables may need re-designed rowid handling.
7. **The @5 `duckdb-extension` world file is byte-identical to @4** (verified: `worlds/duckdb-extension.wit`). The additive interfaces (storage, index, files-reg, secret, settings, etc.) are declared but not in the world — each capability-bearing component defines its OWN world (e.g. `duckdb-extension-storage`). Any tooling that expects a single ambient @5 world listing every capability is wrong; consumer worlds are per-extension.

**Does Section 8 suggest the plan needs revisiting before Phase 1?** — **Yes, one item (risk 1) is potentially plan-invalidating.** A ~2-day spike to confirm `duckdb_create_table_function` can replace `WasmStorageExtension` end-to-end for a real `sqlitewasm` workload should precede Phase 1.

---

## Amendments (2026-07-26, post-spike)

The sqlitewasm C-API viability spike (see `wasm-ecosystem-at-5-spike-storage.md`) resolved Risk 1 and answered the storage-lift question definitively: **read path lifts cleanly to `duckdb_create_table_function`; write path does not exist in the C-API**. Verdict was **GO-WITH-CAVEATS**. The following amendments supersede Decisions 4 and 8 where they conflict with the original text above.

### A1. Decision 4 amended — read via C-API, write via host SQL-text intercept

Original Decision 4 called for lifting both read and write paths to `duckdb_create_table_function`. The spike proved:
- **READ path** via `duckdb_create_table_function` + `duckdb_bind_add_result_column` + `duckdb_init_get_column_index` works cleanly for all 22 core types with plain and projected scans. Aliasing `mydb.foo` works via `ATTACH ':memory:' AS mydb; CREATE VIEW mydb.main.foo AS SELECT * FROM foreign_scan_foo()`.
- **WRITE path** cannot be lifted this way. `INSERT` / `UPDATE` / `DELETE` / `duckdb_appender` all reject views over table functions with `Can only update base table` / `X is not a table`. The C++ `PhysicalOperator` subclass path (`wasm_storage.cpp:1250+`) has no C-API equivalent.

Amended plan:
- **READ path** — lift to host as originally proposed. Delete the read-oriented C++ (`wasm_storage.cpp` scan machinery, `wasm_index.cpp`, `wasm_collation.cpp`, `wasm_files.cpp`, `wasm_component_optimizer.cpp`, `wasm_table_stream.cpp`, `wasm_index_optimizer.cpp`) and the corresponding `*_host` Rust bridges from `core/src/lib.rs`.
- **WRITE path** — NEW. The host adds a SQL-text intercept in `HostState::execute` that recognizes `INSERT INTO <alias>.<table> ...`, `UPDATE <alias>.<table> SET ... [WHERE ...]`, `DELETE FROM <alias>.<table> [WHERE ...]` where `<alias>` matches an attached foreign catalog. Recognized statements are routed to `storage-write-dispatch.{insert,update,delete}` on the owning extension component; unrecognized ones (see A4 below) return a clear `Operation not supported on @5 attached tables` error.
- **`wasm_storage.cpp`'s write shim (`WasmPhysicalInsert/Delete/Update` + `WasmTransaction` + `StorageExtension` bootstrap, ~500 LoC)** — deleted in Phase 1a alongside the read shim. The text-intercept obviates it entirely. Rollback path: build `duckdb-component-core` with `--features storage_host_legacy` to re-add all deleted C++ TUs + Rust bridges; kept for one release cycle as an emergency escape hatch.

### A2. Decision 8 amended — Risk 1 resolved, Risks 2 and 6 confirmed, new Risk 8 introduced

- **Risk 1 (HIGHEST → RESOLVED, mitigation-required):** The C-API supports the read path; the write path is handled by text-intercept. No `storage-host` import survives on the core.
- **Risk 2 (filter pushdown parity):** CONFIRMED — the C-API has no filter-pushdown surface. DuckDB applies `WHERE` post-scan, so every row of the foreign catalog crosses the wasm boundary before filtering. Correctness is fine; expect a 4-5 orders-of-magnitude perf regression on large sqlitewasm scans with selective predicates. Not a Phase 1 blocker; captured as follow-up in the Phase 5 backlog.
- **Risk 6 (`wants_rowid` for UPDATE/DELETE):** CONFIRMED and solved by the text-intercept. UPDATE/DELETE become explicit `storage-write-dispatch.{update,delete}` calls carrying whatever row-identifier the extension itself defines. DuckDB's virtual-rowid never enters the picture.
- **NEW Risk 8 (text-intercept fragility on unusual SQL shapes):** The text-intercept is a first-cut recognizer, not a full parser. Explicit non-goals for v1: `RETURNING`, `INSERT ... ON CONFLICT` / UPSERT, CTE-driven writes (`WITH ... INSERT INTO <alias>.foo ...`), writes referencing an attached alias in both read and write positions in the same statement, multi-statement scripts mixing intercepted and non-intercepted writes, prepared/parameterized DDL. Phase 2 must (a) enumerate the failure shapes explicitly, (b) return the `Operation not supported on @5 attached tables` error consistently rather than silently miscompiling, (c) document the workaround (drop to the native `duckdb` build for the affected query). Not a blocker for `fieldbook` / `mosaic` / basic `sqlitewasm` INSERT/UPDATE/DELETE — those use only the first-cut shapes.

### A3. Decision 5 amended — heavy write-capable extensions get a compat note

`sqlitewasm-component`, `mysqlwasm-component`, `postgreswasm-component`, `unityscan-component` all export `storage-write-dispatch`. Under A1, their write paths flow through the host's text-intercept — no change to the extension WIT surface (dispatch is wire-identical). But their README / release notes must document the v1 non-goals listed in Risk 8 so users don't file bugs for `RETURNING` / UPSERT / CTE-driven writes.

### A4. Phase 1a expanded, Phase 2 expanded

- **Phase 1a delete list** now includes `wasm_storage.cpp`'s write shim in addition to the originally-listed read paths. Net C++ deletion in the core stays at ~3763 LoC — the C++ file itself was ~1497 LoC counted as read; the write shim was already inside that count. Net Rust deletion stays at ~2500 LoC.
- **Phase 2 scope grows by 4-6 days** for the SQL-text write intercept:
  - New `resolve_write_target(sql, attached_aliases) -> Option<WriteRoute>` helper in `HostState`.
  - New route `WriteRoute::{Insert,Update,Delete}` that names (extension-id, foreign-catalog-handle, table-name, column-list-or-WHERE-clause).
  - Wiring into `ExtensionInstance::storage_write_{insert,update,delete}` (already exists in `ducklink-runtime/src/extension.rs` — reuse).
  - Test matrix: happy-path INSERT/UPDATE/DELETE per each heavy extension × explicit rejection tests for each Risk 8 non-goal shape.
- **Phase 2 revised estimate:** 1.5 agent-weeks (up from 1). Total Phase 1–3: still ~2 weeks with 3 agents given the extra work overlaps with existing Phase 2 scope.

### A5. Write-path rowid resolution (2026-07-26, Phase 2 in-flight amendment)

**Gap discovered:** the `storage-write-dispatch.wit` `update` and `delete` variants take `rowids: list<s64>`, not a WHERE predicate. Amendment A1 assumed route-and-forward — reality is that the host must resolve the WHERE clause to concrete rowids before dispatching.

**Decision:** pre-scan resolution. Before dispatching `UPDATE <alias>.<table> SET ... WHERE <pred>` or `DELETE FROM <alias>.<table> WHERE <pred>`, the host issues a synthetic `SELECT rowid FROM <alias>.<table> WHERE <pred>` through the read path, collects the resulting rowids into a `Vec<i64>`, then dispatches `storage_write_update(handle, catalog, table, rowids, new_values)` or `storage_write_delete(handle, catalog, table, rowids)`.

Contract requirements this places on write-capable extensions:
- Their `storage-dispatch.table-columns(table)` must include a synthetic `rowid` column (or an extension-defined stable row-key column named `rowid`). `sqlitewasm`, `mysqlwasm`, `postgreswasm` all expose one already (SQLite has native rowid; MySQL InnoDB has `_rowid`; Postgres via ctid or a synthetic ordinal).
- The rowid must be stable within a transaction: the pre-scan and the write dispatch must see the same rowid space. For v1, all attached-table writes run at DuckDB's default isolation — no explicit BEGIN/COMMIT wrapping the pre-scan + write — so between pre-scan and dispatch, concurrent writers on the foreign side could invalidate rowids. **Documented v1 limitation**; a Phase 5 follow-up would wrap the pair in an extension-side transaction.

Cost: one extra round-trip per write statement (SELECT then INSERT/UPDATE/DELETE). Amortized over WHERE-heavy predicates the pre-scan is cheap; for `DELETE FROM t` (no WHERE), the pre-scan degrades to a full-table rowid enumeration — flag that as a known perf cliff in the extension's README.

Amendment A2's Risk 8 non-goals list is unchanged.

