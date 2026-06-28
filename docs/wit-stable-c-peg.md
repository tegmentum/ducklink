# v3 stable-C peg: which interfaces stand on stable ground

Date: 2026-06-28 (v3 stabilization). This records the "stable interface on an
unstable interface" audit result: which `duckdb:extension` interfaces are pegged to
DuckDB's STABLE C Extension API, and which are an irreducible C++-only tier whose
churn is quarantined in the wasm core shims.

## The two tiers

DuckDB's stable C Extension API (`duckdb_ext_api_v1`) has been frozen since DuckDB
**1.2.0** (PR #14992). It is the only ABI DuckDB promises across versions. Crucially
it has **NOT grown past 1.2.0**: on `main` and on `v1.5.4` the highest stable marker
in `duckdb_extension.h` is `1.2.0`. Everything newer (copy, config-options,
FS-access, catalog-introspection, logger, arrow) is `unstable_*` (version-locks the
binary), and storage / index / optimizer / collation / secret / parser have **no C
anchor at all**.

### COMMON tier -- PEGGED to stable C (`duckdb_ext_api_v1`, >= 1.2.0)

Routed through the `duckdb_create_*` / `duckdb_register_*` C symbols in the wasm
core's `core/src/lib.rs` (verified: `duckdb_create_scalar_function`,
`duckdb_register_scalar_function`, `duckdb_create_table_function`,
`duckdb_register_table_function`, `duckdb_create_aggregate_function`,
`duckdb_register_aggregate_function`, `duckdb_create_cast_function`,
`duckdb_replacement_scan`). A DuckDB version bump does NOT touch these.

| WIT interface | Stable C symbol | 
|---|---|
| `runtime` scalar (+ `runtime-ext`) | `duckdb_create_scalar_function` / `_register_` |
| `runtime` table (+ `table-stream-dispatch`) | `duckdb_create_table_function` / `_register_` |
| `runtime` aggregate (+ `aggregate-incr-dispatch`) | `duckdb_create_aggregate_function` / `_register_` |
| `runtime` cast / `types-ext` casts | `duckdb_create_cast_function` |
| `files` replacement-scan | `duckdb_replacement_scan` |
| `types` (logical types, `complex()` rebuild) | `duckdb_*` vector C API (stable) |

Note: the v3 additions to the common tier (table-fn filter pushdown, window
aggregate+frame) are NEW dispatch SHAPES over the same stable C registration. The
filter/frame plumbing itself uses table-function bind/pushdown C entry points; where
a stable symbol exists the core routes through it.

### ADVANCED tier -- C++-only, BLOCKED ON DUCKDB UPSTREAM

These bind DuckDB's INTERNAL C++ ABI (no stable C symbol exists). They live in the
wasm core's `core/cpp/wasm_*.cpp` and are the entire blast radius of a DuckDB ABI
change. The WIT SHAPE above them is frozen, so a DuckDB bump forces only a core-shim
re-anchor, never a contract bump (the leak audit confirms no internal struct crosses
by value -- `docs/wit-leak-audit.md`).

| WIT interface | DuckDB internal C++ used | Core shim |
|---|---|---|
| `storage` / `storage-dispatch` | `StorageExtension` | `wasm_storage.cpp` |
| `index` / `index-dispatch` | `DBConfig::GetIndexTypes().RegisterIndexType` | `wasm_index.cpp` |
| `optimizer` / `optimizer-dispatch` (v3) | `OptimizerExtension::Register` | `wasm_index_optimizer.cpp` (to be generalized) |
| `parser` / `parser-dispatch` (v3) | `ParserExtension` | (new shim, deferred) |
| `collation` | `Catalog::CreateCollation` | `wasm_collation.cpp` |
| `files` / `file-dispatch` | `FileSystem` (custom VFS) | `wasm_files.cpp` |
| `compression` / `encoding` / `secret` / window-frame | internal C++ registries | core (no stable C) |

## Upstream asks (so the advanced tier can eventually be pegged)

- Parser C-API: DuckDB discussion **#21159** (open).
- A stable C surface for storage / index / optimizer / collation / secret: not yet
  filed -- prerequisite for moving the advanced tier off the internal C++ ABI.

Until DuckDB ships those, the advanced tier stays C++-only by necessity. v3 freezes
the WIT shape over it so that necessity costs us nothing but a localized core-shim
re-anchor per DuckDB bump.
