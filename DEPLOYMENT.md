# Deployment scenarios

The same `duckdb:extension` WebAssembly component — built once — runs unmodified
across three deployment scenarios. The extension binary doesn't change; the host
does. Two cross-cutting **options** (AOT compilation, embedded extensions) layer
on top of the WebAssembly-host scenarios.

## The three scenarios

### 1. Native DuckDB + the `ducklink` extension
Native DuckDB loads the [`ducklink` community extension](native-extension/ducklink/),
which embeds the wasmtime runtime and runs the component inside the native
process. Lets a single portable component extend DuckDB on any platform without
per-platform native extension builds. This is "embed WebAssembly into native
DuckDB" (Direction 2).

- Scalars (all logical types, any arity) and table functions bridge through the
  DuckDB C API; aggregates register via the raw C aggregate API.
- Verified: the Scenario-1 corpus harness
  (`native-extension/ducklink/tests/scenario1_corpus.rs`) loads each extension
  in-process and diffs its `smoke.sql` output against `smoke.expected`.

### 2. Standalone WebAssembly DuckDB host
The `ducklink` host (`crates/ducklink-host`, binary `ducklink`) runs
DuckDB-compiled-to-WebAssembly (the [`duckdb-wasm`](https://github.com/tegmentum/duckdb-wasm)
core) and loads components alongside it, as a native CLI/server. WebAssembly
throughout, no native DuckDB. This is "run a WebAssembly DuckDB that hosts
WebAssembly extensions" (Direction 1).

- Full capability coverage (scalar/table/aggregate/cast/macro/replacement-scan).
- Verified: `make ext-smoke-all` runs every extension's `smoke.sql` through the
  host with golden-output diffing.

### 3. WebAssembly DuckDB in a web browser
The same WebAssembly DuckDB core, running extension components directly in-browser
(the [`web/`](web/) build) via [`@tegmentum/wasi-polyfill`](https://github.com/tegmentum/wasi-polyfill).
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

- **Standalone (native host):** `make precompile` → `ducklink precompile`
  produces `.cwasm` for the core + CLI components; the host loads them via
  deserialize.
- **Browser / Node:** `jco transpile` the core to an AOT module
  (`web/package.json` `verify-tvm-aot`, `web/aot-tvm-test.mjs`).

### Embedded extensions
The "embed framework" compiles pure-Rust algorithm extensions directly into the
core wasm as native scalars (no WIT boundary, native speed) via `embed-<name>`
Cargo features. Applies to both the standalone host and the browser core.

- The embeddable algorithm crates and `embed-<name>` features are **ducklink-side**
  (they did not move to the `duckdb-wasm` core repo in the split). Building a
  core-with-embeds is a ducklink-side overlay over `duckdb-component-core`;
  `make core-embed` is currently a placeholder pending that overlay.

## Capability coverage by scenario

| Capability        | 1. Native ext | 2. Wasm host | 3. Browser |
|-------------------|:-------------:|:------------:|:----------:|
| Scalar functions  | yes           | yes          | yes        |
| Table functions   | yes           | yes          | yes        |
| Aggregate funcs   | yes           | yes          | yes        |
| Cast / macro / replacement-scan | via core | yes | yes |

Scenarios 2 and 3 share the WebAssembly DuckDB core, so they have identical,
full capability coverage. Scenario 1 bridges each capability onto native
DuckDB's C API.
