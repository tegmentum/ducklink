## Building Componentized DuckDB Extensions

This repository ships a template extension (`extensions/sample-extension-component`) and a helper
script that make it easy to scaffold new Wasm components. Use this flow for any extension that you
plan to distribute as a component (as opposed to compiling it directly into DuckDB).

### 1. Scaffold a crate

```bash
cd duckdb-webassembly-component
scripts/new-component-extension.sh my_extension
```

This copies the sample extension into `extensions/my_extension-component` and updates its package
name. The script does **not** edit the workspace automatically, so open the root `Cargo.toml` and
append `extensions/my_extension-component` to the `[workspace].members` list.

### 2. Implement your logic

- Edit `extensions/my_extension-component/src/lib.rs` and replace the sample scalar/table/aggregate
  callbacks with your implementation. The generated bindings in `src/bindings.rs` expose the
  `duckdb:extension/*` interfaces.
- Update `Cargo.toml` (version, dependencies) as needed.
- If the extension needs custom WIT, place it under `extensions/my_extension-component/wit/`.

### 3. Build the component

```bash
cargo component build -p my_extension-component \
  --target wasm32-wasip2 --release
```

The resulting `target/wasm32-wasip2/release/my_extension_component.wasm` is the distributable.
Copy it into `artifacts/extensions/my_extension.wasm` (or any directory scanned by the host).

### 4. Load it through the host

The host runtime (`duckdb-component-host`) automatically looks in `artifacts/extensions`. Launch
the CLI with:

```bash
cargo run -p duckdb-component-host --release -- \
  :memory: --load-extension my_extension -c "select my_extension_function();"
```

This is the **plugin** path: the extension stays a separate `.wasm` artifact and
the core loads it at runtime — nothing is baked into the core.

### 5. Or embed it into the core (`compose`)

The same extension can instead be **compiled into the core component** — it then
registers as a native scalar (no per-row WIT boundary, faster) and works in the
standalone with no host. Embedding is opt-in: by default the core embeds nothing
extra. Add an `embed-<name>` feature to the core crate (`dep:<crate>`; see
`embed-isin`), then select it with a command-line flag (mirrors sqlite-wasm's
`sqlink compose`):

```bash
# list extensions that expose an embed-<name> core feature
ducklink compose --list

# build a core with isin (and others) embedded; optionally precompile to .cwasm
DUCKDB_STATIC_LIB=… DUCKDB_INCLUDE_DIR=… \
  ducklink compose --embed isin --output build/core-isin.wasm --precompile
```

`--embed a,b` maps to `cargo component build … --features wasi,embed-a,embed-b`.
The embed path doesn't disable the runtime loader — `LOAD`/`--load-extension`
still works for everything else. (`make core-embed EMBED=embed-isin` is the
equivalent Make target.)

### Tips

- Keep the sample extension handy as a reference for capability registration and runtime callbacks.
- Declare the capabilities you need (scalar/table/aggregate/pragma/etc.) in your `load()` result so
  the host can enforce the correct permissions.
- If your extension requires assets (e.g., configuration files), package them alongside the `.wasm`
  file and update your `load()` logic to locate them via the WASI preopens.
- Most community extensions today rely on DuckDB’s native APIs instead of the component model. For a
  living list of available community extensions see [duckdb.org/community_extensions](https://duckdb.org/community_extensions/);
  when deciding whether to port one into a Wasm component, focus on the extensions whose surface area
  is limited to scalar/table/aggregate functions so they fit within the current plugin API.
