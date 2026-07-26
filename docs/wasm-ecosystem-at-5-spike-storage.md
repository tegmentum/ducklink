# Spike: can the stable DuckDB C-API replace `WasmStorageExtension`?

**Status:** Complete (2-day spike, ADR Section 8 Risk 1)
**Date:** 2026-07-26
**Scope:** Determine whether `duckdb_create_table_function` + host SQL-router intercept can substitute for the C++ `StorageExtension` subclass path in `duckdb-wasm/core/cpp/wasm_storage.cpp` for a real `sqlitewasm`-style workload.
**Method:** Two self-contained native C programs built against `libduckdb` 1.5.4 (`/opt/homebrew/opt/duckdb`). C-API surface is identical between native and wasm, so native validates.

## Verdict

**GO-WITH-CAVEATS.** Reads migrate cleanly. Writes and filter-pushdown do not. Decision 4 needs a targeted amendment before Phase 1.

## Per-mechanism answers

### (1) ATTACH intercept — PARTIAL

Stable C-API has **no** `duckdb_register_storage_extension`. `ATTACH ... (TYPE <name>)` dispatches via internal `DBConfig::storage_extensions` (C++-only). Exhaustive grep over `duckdb.h` (551 `DUCKDB_C_API` decls): no storage-extension symbol. Spike attempt returned `IO Error: Extension "sqlitewasm.duckdb_extension" not found` because DuckDB fell through to the on-disk resolver.

**Mitigation workable.** Host SQL router text-intercepts `ATTACH ... (TYPE <name>)` before `duckdb_query`; substitutes `ATTACH ':memory:' AS <alias>; CREATE VIEW <alias>.main.<tbl> AS SELECT * FROM foreign_scan_<uid>()` per enumerated foreign table (spike 02 proves this aliasing shape). Text-parse edge cases (multi-statement, `EXPLAIN ATTACH`, DSN quoting, comments) are ~2-3 host-days.

### (2) Read path — plain scan — YES

`duckdb_create_table_function` + `duckdb_bind_add_result_column` + `duckdb_data_chunk_get_vector` round-trips types cleanly. Spike 01: `SELECT * FROM mydb_foo` returns 5 rows over `(INTEGER, VARCHAR, DOUBLE)`. All 22 `duckdb_type` codes mapped in `wasm_storage.cpp:82-135` have C-API `duckdb_create_logical_type` constructors.

### (3) Read path — filtered scan — NO (perf regression only)

**No filter-pushdown API in stable C-API.** `duckdb_init_info` exposes only `duckdb_init_get_column_count` / `_index` (`duckdb.h:4431,4442`) — projection only. No `duckdb_init_get_column_filter`, no `TableFilterSet` accessor. Spike 01: `WHERE id = 42` returns correct 1 row, but `init` logs `column_count = 3` with zero filter info. DuckDB applies WHERE post-scan.

**Regression:** `wasm_storage.cpp:1006-1058` today ships `CONSTANT_COMPARISON` (=, <, <=, >, >=, !=), `IS_NULL`, `IS_NOT_NULL` to `storage-dispatch.scan_open`. All disappear at @5. For a 100k-row `WHERE user_id = 42` scan, expect ~4-5 orders-of-magnitude row-marshalling regression. Correctness OK; perf blocker for large workloads.

### (4) UPDATE path (rowid) — NO

Table functions are not "base tables". Spike 01: `UPDATE mydb_foo SET salary = 999 WHERE id = 42` fails `Binder Error: Can only update base table`; DELETE same. No `COLUMN_IDENTIFIER_ROW_ID` at C-API level. The `wants_rowid` machinery in `wasm_storage.cpp:996-1004` has no C-API home. Today UPDATE/DELETE work because `wasm_storage.cpp:1250+` subclasses C++ `PhysicalOperator` and wires via `Catalog::PlanUpdate`/`PlanDelete` — both C++-only hooks.

### (5) INSERT path — NO

Spike 01: `INSERT INTO mydb_foo VALUES (...)` → `Catalog Error: mydb_foo is not an table`. `INSERT INTO foreign_scan_foo() VALUES (...)` → parser error. Spike 02: `duckdb_appender_create` against the view → `Table ".main.foo" could not be found`. No `duckdb_register_writable_table_function`.

## What blocks GO

**Filter pushdown (3)** is a documented perf regression; not a plan blocker. Acceptable.

**INSERT / UPDATE / DELETE (4, 5)** are the blocker. The four heavy extensions in Decision 5(c) — `sqlitewasm`, `mysqlwasm`, `postgreswasm`, `unityscan` — all export `storage-write-dispatch`. `fieldbook` and `mosaic` both write via sqlitewasm. If those become read-only, half the point of at-5 evaporates.

**Smallest workaround:** host SQL router adds three additional text-intercepts (INSERT / UPDATE / DELETE against an aliased foreign-catalog schema) and routes each to `ExtensionInstance::storage_write_*` (which already exists in `crates/ducklink-runtime/src/extension.rs`). Cost estimate: ~4-6 agent-days for a solid parser (must handle CTEs, RETURNING, multi-VALUES, expression-in-SET, correlated subqueries in WHERE). Alternative: preserve one minimal `storage-host` import on the core plus the ~500 LoC of C++ that implements the three `PhysicalOperator` subclasses (about half of `wasm_storage.cpp`).

## Concrete DuckDB C-API references

| Symbol | Header line | What it proves |
|---|---|---|
| `duckdb_create_table_function` | `duckdb.h:4191` | Read-side entry point works |
| `duckdb_table_function_supports_projection_pushdown` | `duckdb.h:4282` | Projection pushdown available |
| `duckdb_init_get_column_count` / `_index` | `duckdb.h:4431,4442` | ONLY init-time accessors — no filter API alongside them |
| `duckdb_add_replacement_scan` | `duckdb.h:4527` | Migrates cleanly for `FROM 'file.ext'` autoregister |
| `duckdb_appender_create` | `duckdb.h:4631` | Confirmed to reject views (spike 02) |
| (none) | — | No `duckdb_register_storage_extension` |
| (none) | — | No `duckdb_init_get_column_filter` |
| (none) | — | No writable-table-function / virtual-rowid surface |

## Files written

- `spike/01_table_function.c` — proves (2), (3), (4), (5); shows filter is applied post-scan, rejects UPDATE/DELETE/INSERT with exact DuckDB error strings.
- `spike/02_schema_aliasing.c` — proves the `ATTACH ':memory:' AS mydb; CREATE VIEW mydb.main.foo AS SELECT * FROM foreign_scan_foo()` aliasing shape gives natural `mydb.foo` resolution; confirms appender rejects views; confirms real base tables in the mem catalog take writes fine.

(Spike dir: `/private/tmp/claude-501/.../scratchpad/at5-storage-spike/`)

## Recommended ADR amendments

- **Decision 4** — split storage into a read path (C-API-only, as written) and a write path (currently unaddressed). Do not delete `wasm_storage.cpp` in one shot; delete its `WasmCatalog`/`WasmSchemaEntry`/scan machinery (~1000 LoC), keep `WasmPhysicalInsert/Delete/Update` + `WasmTransaction` + the `StorageExtension` bootstrap (~500 LoC) as a "write-only shim" until the host router grows write intercepts.
- **Decision 4** — add explicit acknowledgement that ATTACH interception happens at the HOST SQL-router level (text parse), not at DuckDB parse time. List the edge cases the router must handle: multi-statement scripts, `EXPLAIN ATTACH`, comments, quoted DSN.
- **Decision 4** — filter pushdown is DELETED at @5. Add a perf-regression note. Optional Phase 5 item: expose filter info via a WIT-level scan-hint call from the host to `storage-dispatch.scan_open` based on the host's own parse of the WHERE clause. Only pays off if the host is already parsing WHERE for the write-intercept path.
- **Decision 5** — 4 heavy extensions (`sqlitewasm`, `mysqlwasm`, `postgreswasm`, `unityscan`) will lose UPDATE/DELETE unless the write-shim survives. Update the "extensions that fully break at @5" count from 0 to 4-if-write-shim-dropped.
- **Decision 7 Phase 2** — add "host write-SQL intercept" as a new ~1 agent-week sub-phase between the storage read shim and Phase 3, OR keep the C++ write shim and mark it as "surviving legacy" in Phase 1.
- **Decision 8 Risk 1** — confirmed as partial-blocker by this spike; upgrade from "HIGHEST" to "resolved, mitigation-required".
- **Decision 8 Risk 2** — confirmed: filter pushdown is completely gone, not merely restricted.
- **Decision 8 Risk 6** — confirmed: `wants_rowid` has no C-API home; the write path cannot use it.
