# Repository Guidelines

## Project Structure & Module Organization
- `crates/duckdb-core-component/`: Rust host for the DuckDB engine packaged as a Wasm component (core database APIs).
- `crates/duckdb-cli-component/`: CLI wrapper exporting `wasi:cli/run` for interactive use.
- `crates/libduckdb-sys/`: FFI bindings and build script that vendors the prebuilt DuckDB static library (`artifacts/libduckdb-wasi.a`).
- `external/`: Upstream sources and toolchains (DuckDB, wasi-sdk). Treat as read-only.
- `scripts/`: Helper scripts for building the DuckDB static library, syncing WIT files, and smoke-testing.
- `docs/`: Design notes and future plans (e.g., `component-plugin-plan.md`).

## Build, Test, and Development Commands
- `export WASI_SDK_PREFIX=<repo>/duckdb-webassembly-component/external/wasi-sdk-28.0-<platform> && DUCKDB_STATIC_LIB=... && DUCKDB_INCLUDE_DIR=...`: Required environment variables before any build (see README for exact paths). Set `WASI_TARGET_TRIPLE=wasm32-wasip2` when compiling the DuckDB archive so it matches the component target. Use `WASM_EXTENSIONS` (defaults to `json`) to control which built-in DuckDB extensions are compiled into the static library.
- `cargo component build -p duckdb-core-component --target wasm32-wasip2 --release --features "wasi fs_shims"`: Builds the core Wasm component with filesystem shims.
- `cargo component build -p duckdb-cli-component --target wasm32-wasip2 --release`: Builds the CLI component.
- `scripts/smoke-cli.sh`: Composes core + CLI via `wac plug` and runs a Wasmtime sanity query (`SQL` and `DB_PATH` env vars configurable).
- `cargo check -p duckdb-core-component`: Fast validation of Rust sources when full component builds are unnecessary.

## Coding Style & Naming Conventions
- Rust code follows `rustfmt` defaults (run `cargo fmt` before committing). Use snake_case for functions, CamelCase for types, SCREAMING_SNAKE_CASE for constants.
- Keep module-level logging concise (`eprintln!` for temporary diagnostics, gate permanent logging behind feature flags).
- WIT packages live under `crates/*/wit/` and follow `snake-case` directories; update via `scripts/sync-*-wit.sh` when WIT changes upstream.

## Testing Guidelines
- Primary smoke coverage is via `scripts/smoke-cli.sh` (Wasmtime + `wac`). Ensure static libraries are up to date before running.
- Add unit tests in Rust with `#[cfg(test)]` modules; prefer lightweight component-free tests (`cargo test -p duckdb-core-component`).
- For new WIT interfaces, add integration checks to smoke script or create specialized scripts in `scripts/`.

## Commit & Pull Request Guidelines
- Commit messages typically follow `<scope>: <short description>` (e.g., `core: add wasi fs shim`); keep the first line ≤ 72 chars.
- Include descriptive bodies for behavior changes, referencing issue numbers (`Fixes #123`) when applicable.
- PRs should summarize changes, list testing performed (`cargo component build`, `scripts/smoke-cli.sh`), and highlight follow-ups.
- Attach logs or Wasmtime output for failures; mention environment variables used (`WASI_SDK_PREFIX`, `DUCKDB_STATIC_LIB`).
