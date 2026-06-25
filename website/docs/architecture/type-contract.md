---
id: type-contract
title: The type-contract evolution
sidebar_label: Type contract
---

# The type-contract evolution

The richness of the [logical type system](capability-surface.md) is itself part
of the `duckdb:extension` contract. This page records *why* extending it is a
contract change, the canonical-ABI finding that follows, and the escape hatch for
nested types.

## Adding a type is a contract bump

The WIT `types.wit` started with only `boolean / int64 / uint64 / float64 / text
/ blob`, with `duckvalue` matching. Every component flattened real types (dates,
timestamps, decimals, lists) to text, capping data fidelity for **all**
components.

The fix was purely additive and mechanical — extend `logicaltype` + `duckvalue`
with the integer/unsigned family, `date`, `time`, `timestamp`, `timestamptz`,
`interval`, `decimal(width, scale)`, and `uuid`, then touch every conversion site
(`convert_extension_logicaltype`, `convert_*_duckvalue`, `write_duckvalue_to_vector`,
`neutral_logicaltype_to_core`). But there is a catch:

:::info The canonical-ABI finding
Adding a case to the shared `capability` variant — or new types to `types` /
`callback-dispatch` — **changes the structural type** that every component
imports/exports. Because each of the ~159+ components links against its own
*frozen* copy of the WIT, a types change is a **full-catalog rebuild**, and it
risks instantiation breakage for any component that doesn't match. A types bump is
a contract bump.
:::

This is why new *capabilities* (storage, files, index) are added as **separate
interfaces and a separate world** that a component opts into — leaving the shared
`runtime` / `types` / `callback-dispatch` interfaces byte-identical — while
*type-system* changes, which can't be isolated, are batched and applied as a
deliberate catalog-wide rebuild. Two such type bumps have been applied (the
second adding decimal/interval/uuid and a builtin GEOMETRY arm; real C type codes
INTERVAL=15 / DECIMAL=19 / UUID=27, with DECIMAL special-cased to width-38/int128
and a UUID sign-bit flip), each verified with full smoke (171/171).

## The escape hatch — nested types are infeasible by-value

Nested `list` / `struct` types are the one place the append-a-case pattern hits a
hard wall:

:::warning WIT prohibits recursive data types
Adding `list<duckvalue>` or `struct(record { value: duckvalue })` to the
`duckvalue` variant fails the wit-parser cycle check (`type duckvalue depends on
itself`) on every form tried — direct, list-wrapped, or alias-indirected. Only
WIT `resource` handles may recurse, but that is a host-owned-handle model
incompatible with the frozen by-value contract (`resultset = list<list<duckvalue>>`).
:::

So nested types are feasible only via either:

1. **A non-recursive flattened encoding** — a `complex` / `nested(type-expr-string,
   value-text/json)` case the core reconstructs into a real `LIST`/`STRUCT`
   vector. The type string is parsed via `CAST(NULL AS <expr>)`; the value via
   DuckDB's native list/struct text cast. This is the **escape hatch**: a single
   opaque case that carries arbitrarily complex shapes as a `(type-expr, value)`
   pair rather than recursing the variant.
2. **A resource-handle value-model redesign.**

Both are larger design changes than appending a case, so they are deferred — and,
crucially, **not needed for correctness today**: a component can already return
nested data as text/JSON plus an explicit `::INTEGER[]` cast, with zero new
surface.

## Outcome

The deferred type items resolved cleanly. The custom-index keystone proved
**general** — one WIT serves both HNSW point-kNN and R-tree bbox-intersection
(`rtreefns` backs `CREATE INDEX … USING wasm_rtree` with zero core change), and
the optimizer rewrite swaps a `seq_scan` GET for a real
`wasm_hnsw_index_scan` TableFunction. The remaining open questions are design
choices (the flattened-encoding vs resource redesign for nested types, and
GDAL/PROJ `ST_Transform` as an optional engineering add), not bugs.
