---
id: deployment
title: Deployment scenarios
sidebar_label: Deployment
---

# Deployment scenarios

The same `duckdb:extension` WebAssembly component — **built once** — runs
unmodified across three deployment scenarios. The extension binary doesn't change;
the host does. Two cross-cutting **options** (AOT compilation, embedded
extensions) layer on top of the WebAssembly-host scenarios.

## The three scenarios

### 1. Native DuckDB + the `ducklink` extension

Native DuckDB loads the `ducklink` extension (a loadable `.duckdb_extension`,
**v0.4.0**), which embeds the Wasmtime runtime and runs the same
[`@4.0.0`](../architecture/columnar-abi.md) component inside the native process.
This lets a single portable component extend DuckDB on any platform without
per-platform native extension builds — "embed WebAssembly into native DuckDB." It
is built **only against DuckDB's stable C Extension API** (`duckdb_ext_api_v1`),
so it is not version-locked to DuckDB internals.

The native extension ships in **tiers**:

- **Common tier (shipped).** Scalar (all logical types, any arity), table
  functions (with projection pushdown), aggregates (via the raw C aggregate API),
  and **window** over those aggregates — all bridge through the stable C API.
- **Advanced tier (in progress).** Parser, general optimizer, and table-function
  **filter** pushdown have **no stable C anchor** — they need DuckDB's internal
  C++ ABI via a core C++ shim, a planned follow-on. They are **deferred** natively
  today, so native does **not** yet have full parity with the wasm host.

:::note Community-extensions status
The community-extensions submission is **held pending full parity** — the native
extension is not published to the DuckDB community-extensions registry, and native
parity is incomplete (the advanced tier is deferred). Treat native as "common tier
shipped, advanced tier in progress."
:::

- Verified by the Scenario-1 corpus harness (`tests/scenario1_corpus.rs`), which
  loads each extension in-process and diffs its `smoke.sql` output against
  `smoke.expected`.

### 2. Standalone WebAssembly DuckDB host

The `ducklink` host (`crates/ducklink-host`, binary `ducklink`) runs
DuckDB-compiled-to-WebAssembly (the
[`duckdb-wasm`](https://github.com/tegmentum/duckdb-wasm) core) and loads
components alongside it, as a native CLI/server. WebAssembly throughout, no native
DuckDB — "run a WebAssembly DuckDB that hosts WebAssembly extensions."

- Full capability coverage (scalar / table / aggregate / cast / macro /
  replacement-scan).
- Verified by `make ext-smoke-all`, which runs every extension's `smoke.sql`
  through the host with golden-output diffing.

### 3. WebAssembly DuckDB in a web browser

The same WebAssembly DuckDB core, running extension components directly in-browser
(the `web/` build) via
[`@tegmentum/wasi-polyfill`](https://github.com/tegmentum/wasi-polyfill).
Extensions ship and run client-side with zero install. Same full capability
coverage as scenario 2 (the browser reuses the same JS extension host + core
wasm).

- Verified headless (Playwright): `cd web && node corpus-verify.mjs` runs every
  extension's `smoke.sql` in the in-browser DuckDB.

## Cross-cutting options

These are build/packaging variants of the WebAssembly-host scenarios (2 and 3);
they do not change the component.

### AOT compilation

The core wasm's Cranelift compile (~7 s) otherwise happens on every cold start.
Precompiling skips it (load via deserialize, ~0.1 s). AOT artifacts are CPU- and
runtime-version specific — regenerate per target.

- **Standalone (native host):** `make precompile` → `ducklink precompile` produces
  `.cwasm` for the core + CLI components; the host loads them via deserialize.
- **Browser / Node:** `jco transpile` the core to an AOT module.

### Embedded extensions

The "embed framework" compiles pure-Rust algorithm extensions directly into the
core wasm as native scalars (no WIT boundary, native speed) via `embed-<name>`
Cargo features. Applies to both the standalone host and the browser core. The
embeddable algorithm crates and `embed-<name>` features are ducklink-side; see
[writing a component](writing-a-component.md#6-or-embed-it-into-the-core-compose).

## Capability coverage by scenario

| Capability | 1. Native ext | 2. Wasm host | 3. Browser |
|---|:---:|:---:|:---:|
| Scalar functions | yes | yes | yes |
| Table functions | yes | yes | yes |
| Aggregate functions | yes | yes | yes |
| Cast / macro / replacement-scan | via core | yes | yes |

Scenarios 2 and 3 share the WebAssembly DuckDB core, so they have identical, full
coverage. Scenario 1 bridges each capability onto native DuckDB's C API.
