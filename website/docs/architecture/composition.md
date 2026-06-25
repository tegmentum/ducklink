---
id: composition
title: Inter-component composition
sidebar_label: Composition
---

# Inter-component composition

Because every extension is a WebAssembly component with a typed WIT interface,
components **compose**: one component can plug another with
[`wac`](https://github.com/bytecodealliance/wac), producing a single composite
`.wasm` that the host loads exactly like a leaf component. This lets a small
"facade" extension reuse a heavy native library that lives inside another
component, behind the WIT boundary.

## The GDAL example — `spatialproj` composes `gdal`

`spatialproj` exposes a single `ST_Transform` scalar (coordinate reprojection),
but the real work — GEOS / PROJ / GDAL — is a large native C/C++ stack. Rather
than statically linking that stack into every spatial extension, it is built once
into a `gdal` component (the [C-lib-in-wasm pattern](capability-surface.md#the-two-foundational-techniques),
with `PROJ` and the `proj.db` layout embedded), and `spatialproj` **composes** it:

```bash
# extensions/spatialproj-component/compose.sh, in essence:
wac plug spatialproj.wasm --plug gdal.wasm -o spatialproj.composed.wasm
```

The composed artifact registers `ST_Transform`, calls into GDAL/PROJ across the
internal component boundary, and is loaded with `LOAD spatialproj` like any other
component. The composition is recorded for [embedding
tracking](../guides/embedding-tracking.md): `spatialproj` *composes* `gdal`, and
`gdal` *embeds* `PROJ` / `proj.db`.

This is the same mechanism behind the [storage pushdown
work](../capabilities/storage-pushdown.md): a heavy C library (SQLite-C-in-wasm)
is a durable WIT component that other parts of the system drive over a stable
interface, never coupling to DuckDB's internal C++ ABI.

## Why composition matters

- **Reuse without re-linking.** The expensive native stack is built once; facade
  extensions plug it instead of re-bundling it.
- **Stable seams.** The seam between facade and library is a WIT interface, so
  each side can evolve independently.
- **Same loading story.** A composed component is just a `.wasm` — `LOAD` it,
  embed it, or run it in the browser like any leaf component.
