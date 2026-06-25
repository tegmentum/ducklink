---
id: index
title: Capabilities reference
sidebar_label: Overview
slug: /capabilities
---

# Capabilities reference

This section documents the individual capability surfaces a component can
implement, with their WIT shapes, host wiring, and verification. For the
high-level model, start with [the capability surface](../architecture/capability-surface.md).

The full `duckdb:extension` world exposes: **scalar**, **table**, **aggregate**,
**cast**, **macro**, **collation**, **pragma** (+ `spi.run-sql`), **catalog**
(ATTACH), **files** (virtual FileSystem), and **index** (+ optimizer), over a
**rich type system**. All are implemented and verified in-sandbox on the lean
core.

- **[Storage pushdown + catalog ATTACH](storage-pushdown.md)** — the `storage`
  interface that backs `ATTACH … (TYPE …)` with projection/filter pushdown,
  proven by a SQLite-C-in-wasm component.
- **[Catalog, files & cast registrations](catalog-files-casts.md)** — the
  `catalog` and `files` interfaces (macros, logical types, casts, replacement
  scans, copy handlers) and exactly which map onto a real DuckDB C-API path.

## The remaining-capabilities program (complete)

Five capability surfaces remained after the [de-embed program](../architecture/lean-core.md)
delivered everything that fit the existing capabilities. All five are now
implemented and verified:

| # | Capability | What it enables | Notes |
|---|---|---|---|
| 1 | **Richer logical types** | dates/timestamps/decimal/uuid with full fidelity | the one contract bump — see [type contract](../architecture/type-contract.md) |
| 2 | **Collation registration** | `ORDER BY x COLLATE icu_sv` | reuses the scalar callback |
| 3 | **Custom index (+ optimizer)** | `CREATE INDEX … USING HNSW`/R-tree and the planner *choosing* it | the keystone; serves both vss and spatial |
| 4 | **PRAGMA that generates SQL** | `PRAGMA create_fts_index(...)` | needs the `spi.run-sql` host import |
| 5 | **GEOMETRY type + GDAL/PROJ** | first-class spatial type + reprojection | heaviest; builds on 1 + 3 |

The keystone was Item 3's **optimizer integration** — the planner auto-rewriting
`ORDER BY array_distance(col, q) LIMIT k` into a wasm component's HNSW index scan.
Item 5's R-tree (`rtreefns`) then proved the index WIT is general: the same
interface serves HNSW point-kNN and R-tree bbox-intersection with zero core
change.
