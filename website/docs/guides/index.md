---
id: index
title: Guides
sidebar_label: Overview
slug: /guides
---

# Guides

Task-oriented guides for working with ducklink.

- **[Writing a component extension](writing-a-component.md)** — scaffold,
  implement, build, smoke, and optionally embed an extension.
- **[Building & the lean core](building.md)** — prerequisites, cross-compiling
  `libduckdb` to wasm, building the components, and selecting what the core
  embeds.
- **[Embedding tracking (builds / bundles)](embedding-tracking.md)** — record
  what each build embeds, content-hashed.
- **[Function prefixes](prefixes.md)** — SPARQL-style namespacing for colliding
  function names.
- **[The HTTP server (`ducklink serve`)](serve.md)** — serve SQL over HTTP/HTTPS
  with a database-driven router.
- **[The JavaScript/TypeScript APIs](javascript.md)** — the `@tegmentum/ducklink`
  / `sqlink` / `datalink-browser` packages: `create` / `connect` / `query` /
  `load` and a typed `Result`.
- **[Extension distribution (R2)](distribution.md)** — serving components from
  Cloudflare R2, the `catalog.json` model, and how each client points at it.
- **[Deployment scenarios](deployment.md)** — the same component across native,
  standalone, and browser hosts.
