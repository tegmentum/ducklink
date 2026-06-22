# ducklink (DuckDB community extension)

Run WebAssembly **component** extensions inside DuckDB.

A `duckdb:extension` component is built once and runs unmodified on every
platform DuckDB supports — no per-platform native extension builds. `ducklink`
embeds [wasmtime](https://wasmtime.dev), loads the component, runs its `load()`
to discover the functions it registers, and bridges them into DuckDB's catalog.

```sh
# Name the component(s) to expose, then LOAD ducklink registers their functions.
DUCKLINK_COMPONENTS=sample=/path/sample_extension.wasm \
  duckdb -unsigned -c "LOAD 'ducklink.duckdb_extension'; SELECT sample_plus_one(41);"
```

Catalog registration happens at extension-load time (DuckDB's model), so the
components are named up front via `DUCKLINK_COMPONENTS` (a `:`-separated list of
`name=path` or `path`) rather than an in-query `CALL`.

## Three deployment scenarios, one component

The same `duckdb:extension` component artifact, built once, runs unmodified in
three deployments:

1. **Native DuckDB + the `ducklink` extension** (*this crate*) — native DuckDB
   loads `ducklink`, which embeds the wasmtime WebAssembly runtime and runs the
   component inside the native process. Lets a single portable component extend
   DuckDB on any platform without per-platform native extension builds.
2. **Standalone WebAssembly DuckDB** — the `ducklink` host runs
   DuckDB-compiled-to-WebAssembly and loads components alongside it, as a native
   CLI/server. WebAssembly throughout, no native DuckDB.
3. **WebAssembly DuckDB in a web browser** — the same WebAssembly DuckDB build,
   running extension components directly in-browser (the `web/` build). Extensions
   ship and run client-side with zero install.

Scenario 1 is "embed WebAssembly into native DuckDB"; scenarios 2 and 3 are
"run a WebAssembly DuckDB that hosts WebAssembly extensions" — natively and in
the browser respectively.

All three share the [`ducklink-runtime`](../../crates/ducklink-runtime) engine
crate: the `duckdb:extension` wasmtime bindings, the neutral `reg::*`
registration model, and the callback registry. A component therefore loads
identically in every scenario.

## Layout

- `src/engine.rs` — the direction-agnostic engine glue: `Engine2::load` loads a
  component, runs its `load()`, and returns the `ScalarFunc`s it registered;
  `Engine2::dispatch_scalar` routes a DuckDB invocation back into the component
  through the shared callback registry. Depends only on `ducklink-runtime` +
  wasmtime, so it builds and is checked **without** the DuckDB toolchain.
- `src/lib.rs` — the `loadable` module (behind the `loadable` feature) holds the
  DuckDB C-API binding: the extension entry point and the per-function
  registration that maps a `ScalarFunc` onto a DuckDB scalar function.

## Build

The default build checks the engine glue against `ducklink-runtime`:

```
cargo check          # engine.rs, no DuckDB toolchain needed
```

The loadable artifact builds for the **native** host triple via the DuckDB Rust
C Extension API (`build: cargo`), separately from the wasm component workspace:

```
cargo build --features loadable --release
```

The community-extensions CI builds it with the `rust` and `python3` toolchains.
It is excluded from the `wasm_*` platforms (it embeds a JIT) and from the
static-musl / mingw triples.

## Status

Working and verified end-to-end against a real in-process DuckDB (the `bundled`
test): `LOAD ducklink` loads each `DUCKLINK_COMPONENTS` entry, registers its
scalar functions, and `SELECT fn(x)` dispatches every row into the wasm
component (`SELECT sample_plus_one(41)` → 42, computed in wasm).

Coverage:
- **Scalar functions** — any arity, all logical types
  (`INT64`/`UINT64`/`DOUBLE`/`BOOLEAN`/`VARCHAR`/`BLOB`). One dynamic `WasmScalar`
  serves every signature (the per-function signature is fed to the static
  `VScalar::signatures()` via a thread-local set during registration).
- **Table functions** — verified end to end (`SELECT * FROM sample_emit_sequence(5)`
  streams rows from the component through a `VTab` bridge).
- **Aggregate functions** — not bridged: duckdb-rs exposes no safe aggregate API
  (would require raw C-FFI). Components' aggregate registrations are captured but
  not registered; this is the one known gap.

Packaging the `loadable` cdylib (which exports `ducklink_init_c_api`) as a
loadable `.duckdb_extension` — the metadata footer + a DuckDB-version-matched
build — is handled by the community-extensions CI.
