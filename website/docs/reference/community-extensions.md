---
id: community-extensions
title: Community extensions on wasm
sidebar_label: Community extensions
---

# DuckDB community extensions on wasm — feasibility triage

:::note Status
The feasible pure-wasm component set is built out. The [catalog](../catalog.md)
ships the working component extensions; `tooling/smoke.py --all` verifies them.
Every dep-free and header-only candidate below has been delivered as a Rust
component (often via the same upstream crates), e.g. `hashfuncs` ← xxhash/murmur,
`json_schema` ← json-schema-validator, `stochastic` ← boost-math (statrs),
`celestial`, `bitfilters`, `tsid`, `crypto`. What remains is genuinely blocked or
deferred, not skipped.
:::

The [DuckDB Community Extensions registry](https://duckdb.org/community_extensions/list_of_extensions)
holds 262 third-party extensions (`h3`, `lindel`, `crypto`, `markdown`, …). Most
are **C++** (some Rust), so — like the official extensions — they can't be
runtime-loaded on wasip2 (no dynamic linking). Each would need to be statically
compiled against `libduckdb-wasi` and embedded via `EMBED_EXTENSIONS`.

## What actually gates feasibility

1. **A DuckDB-1.4 (`andium`) ref** — the commit the community CI built against
   DuckDB 1.4 (our version). DuckDB's C++ extension ABI is version-locked and
   unstable *even across patch releases*, so without a 1.4-matched ref the source
   won't compile. **Only 71 of 262 have one** — the real ceiling without
   per-extension version-matching work.
2. **Native dependencies — effort, not a blocker.** Header-only deps get vendored;
   compiled libs get a wasi-sdk build wired in via a `<ext>-deps.cmake` that
   replaces the `find_package` — the same pattern used for the official
   extensions (`spatial` → GEOS/PROJ/GDAL, `httpfs` → openssl/curl).
3. **Rust — not a blocker.** Rust extensions build for `wasm32-wasip2`
   first-class (this repo already ships the delta kernel + the component
   extensions that way).
4. **Runtime primitives — the only real blocker.** Sockets are available, but
   threads, fork/exec/subprocess, and most raw syscalls are not. Extensions that
   fundamentally need them (`shellfs`, `system_stats`, `sshfs`) are the
   genuinely-infeasible set — a small minority.

:::tip Not a gate: the `excluded_platforms: wasm_*` flag
That flag skips DuckDB's **emscripten** community-wasm CI — a different toolchain
and runtime from the **wasip2 component model**. The most common exclusion reason
is "wraps a Rust crate," which emscripten can't build but wasip2 builds
first-class. So Rust extensions excluded from emscripten are often *more* feasible
here, not less.
:::

## Triage summary

71 of 262 carry a DuckDB-1.4 ref. By the real wasip2 gate (emscripten exclusion
ignored): 56 C++ (static-link), 9 Rust+C++ mixed, 3 C++ + vcpkg native dep, 2 need
a python build tool, 1 pure Rust. The other 191 have no 1.4 ref — revisitable only
with per-extension version-matching.

## Candidates by native-dependency tier

- **Tier A — truly dep-free** (pure DuckDB C++ API): `read_lines`, `func_apply`,
  `duck_hunt`, `scalarfs`, `parser_tools`, `celestial`, `duck_block_utils`,
  `tsid`, `bitfilters`, `quickjs`. (Some query-farm / quackscience extensions ship
  `query_farm_telemetry.hpp`, which phones home on load — undesirable under wasi;
  prefer the no-telemetry ones or patch it out.)
- **Tier B — header-only vcpkg dep** (vendorable): `hashfuncs`, `rapidfuzz`,
  `json_schema`, `stochastic`.
- **Tier C — compiled-lib vcpkg dep** (needs a wasi build of the lib):
  `markdown`, `marisa`, `magic`, `ion`, `textplot`, `yaml`, `datasketches`,
  `duckpgq` (needs openssl — already have openssl-wasm).
- **Network-bound** (any tier): `http_client`, `http_request`, `cronjob`,
  `web_archive`, `web_search`, `open_prompt`, `webmacro`, `netquack`,
  `cloudfront` — possible via the `wasi:sockets` graft, but deferred.

## The way ducklink delivers these — as components, not static C++

Static-linking a C++ community extension *works*, but inherits DuckDB's
version-locked C++ ABI — the very treadmill above. So the **standard is to deliver
the functionality as a Rust component** (the `duckdb:extension` WIT world), like
every other in-repo extension. Components have a stable WIT contract, are one
portable `.wasm`, run as a runtime `LOAD` *or* `ducklink compose --embed`, and
**don't break across DuckDB versions**. The C++ static-link route was reverted.

This catalog therefore reads as a **functionality map**: useful behaviour to
reimplement as components (often with the same upstream Rust crates), not a list
of C++ binaries to chase.

```bash
# scaffold a component that reimplements the functionality:
python3 tooling/scaffold.py crypto --crate sha1,sha2,sha3,blake3,crc32fast
make ext NAME=crypto-component        # build + smoke
ducklink compose --embed crypto       # optional: bake it into the core
```

## The deeper signal: DuckDB's C++ extension mechanism is brittle

- **No stable ABI** — extensions compile against a specific DuckDB version's C++
  headers and break on changes, even across patch releases.
- **A per-version, per-platform rebuild treadmill** — 191 of 262 have no
  DuckDB-1.4 ref at all.
- **Platform-locked binaries** — loadable `.duckdb_extension` files are
  arch/OS-specific.

**Why this is not a blocker for ducklink:** the product contract is the **WIT
interface + the wasm runtime**, not the DuckDB C++ API. The DuckDB version built
against is an internal detail behind that contract, so the community's lag is
*their* treadmill, not ours. The durable path is the component model: a stable,
versioned contract, one portable `.wasm` per extension, no recompile-per-DuckDB-version.
