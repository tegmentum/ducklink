---
id: index
title: Reference
sidebar_label: Overview
slug: /reference
---

# Reference

Status references for the extensions ducklink builds against, and the Iceberg
surface.

- **[Official extensions on wasm](official-extensions.md)** — DuckDB's own C++
  extensions, how they are statically linked, and the per-extension wasm
  feasibility verdict.
- **[Community extensions on wasm](community-extensions.md)** — feasibility
  triage of the DuckDB Community Extensions registry, and why the durable path is
  reimplementing functionality as components.
- **[Iceberg](iceberg.md)** — the Apache Iceberg surface on wasm: what works,
  what's deferred, and the upstream gaps.
- **[Performance](performance.md)** — the measured native-vs-wasm overhead, what
  the `@4.0.0` columnar ABI changed, and an honest read on what is not yet
  measured.
- **[Extension roadmap](extension-roadmap.md)** — the original batched roadmap of
  functionality to deliver as components.
