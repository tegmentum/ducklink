# Component-Based Extension Loading Plan

> Status (2026-06): the loader described below is **implemented and working**
> through the native host (`ducklink-host`). `ensure_extension_loaded`
> instantiates the extension component with wasmtime, runs its `load()`, drains
> the captured scalar/table/aggregate registrations, and forwards them to the
> core component so `--load-extension <name>` registers real DuckDB functions.
> Verified end-to-end with `sample-extension-component`. See `CURRENT_TASK.md`
> for the run recipe and `docs/PLAN-capability-migration.md` for parked work.

We want DuckDB extensions to load as WebAssembly components instead of native
shared libraries. High-level steps:

1. **Package extensions as components**
   - Build each extension (e.g. `parquet`, `json`, `httpfs`) as a standalone
     component exporting the C API entry points (`duckdb_extension_init`, etc.).
   - Define a small WIT interface for extension modules (init/config entry).
   - Extend build scripts to emit `<name>.wasm` alongside the core/cli
     components (similar to duckdb-wasm’s `.duckdb_extension.wasm`).

2. **Implement a component loader in the Rust shim**
   - Intercept DuckDB’s `ExtensionHelper::LoadExtensionInternal` via the Rust
     shim (e.g., register a custom extension callback).
   - Resolve an extension name to a component file under
     `artifacts/extensions/<name>.wasm`. *(Registry wiring now records each
     registration and sanitizes names into this directory; loader still needs
     to instantiate the component.)*
   - Instantiate the component with wasmtime, wiring imports (filesystem,
     logging, network) from the host.
   - Call the exported `duckdb_extension_init` using DuckDB’s C API pointers so
     the extension registers normally.
   - Cache component instances for repeated `LOAD foo` calls.

3. **Expose hooks to DuckDB**
   - Populate `config.extension_callbacks` so DuckDB queries our loader first.
   - Pre-register standard extensions so `AutoLoadExtension` resolves via the
     component loader.

4. **Build/test updates**
   - Extend scripts (e.g. `smoke-cli.sh`) to compose core + cli + extensions and
     verify `LOAD` + functionality under wasmtime.
   - Once `json` (or another extension) is working, replicate the pattern for
     remaining built-ins.

This plan is parked here so we can resume the main DuckDB WebAssembly build
work and return to extension componentization later.
