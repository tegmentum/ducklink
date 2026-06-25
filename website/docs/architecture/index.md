---
id: index
title: Architecture
sidebar_label: Overview
slug: /architecture
---

# Architecture

ducklink is built around one idea: **DuckDB extension functionality belongs in
portable WebAssembly components behind a stable WIT contract, not statically
linked into a binary against DuckDB's unstable C++ ABI.** This section explains
the machinery that makes that work.

- **[The `duckdb:extension` capability surface](capability-surface.md)** — the
  full set of capabilities a component can implement (scalar, table, aggregate,
  cast, macro, collation, pragma, catalog/ATTACH, files/FileSystem,
  index + optimizer, custom type) over a rich logical type system, and how each
  is dispatched.
- **[The lean core + de-embed program](lean-core.md)** — why the core embeds
  almost nothing, and how official extensions move out of the core and into
  components.
- **[Inter-component composition](composition.md)** — how one component plugs
  another (`wac`), with the GDAL `ST_Transform` example.
- **[The type-contract evolution](type-contract.md)** — why adding a logical
  type is a contract change, the canonical-ABI finding, and the escape hatch for
  nested types.
