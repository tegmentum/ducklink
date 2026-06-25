---
id: writing-a-component
title: Writing a component extension
sidebar_label: Writing a component
---

# Writing a component extension

Extensions live under `extensions/<name>-component`, register imperatively in
`load()` against the `duckdb:extension` world, and are tracked by the tooling in
`tooling/` + `registry/`. This guide walks the scaffold → implement → build →
smoke loop. `isin` (hand-rolled) and `baseN` (crate-backed) are worked examples.

## 1. Scaffold a crate

The scaffold consults `tooling/compat-registry.json` for crate status, registers
the workspace member, and cargo-checks that it compiles:

```bash
make ext-scaffold NAME=myext CRATE=base32,bs58
```

There is also a lower-level helper that copies the sample extension into
`extensions/my_extension-component` and updates its package name (it does **not**
edit the workspace, so append `extensions/my_extension-component` to the root
`Cargo.toml` `[workspace].members` yourself):

```bash
scripts/new-component-extension.sh my_extension
```

## 2. Implement your logic

- Edit `extensions/myext-component/src/lib.rs` and replace the sample
  scalar/table/aggregate callbacks with your implementation. The generated
  bindings in `src/bindings.rs` expose the `duckdb:extension/*` interfaces.
- Update `Cargo.toml` (version, dependencies) as needed.
- If the extension needs custom WIT, place it under
  `extensions/myext-component/wit/`.
- **Declare the capabilities you need** (scalar / table / aggregate / pragma /
  cast / …) in your `load()` result so the host can enforce the correct
  permissions. See [the capability surface](../architecture/capability-surface.md).

## 3. Build the component

```bash
# via the make target (build + smoke):
make ext NAME=myext-component

# or directly with cargo-component:
cargo component build -p myext-component --target wasm32-wasip2 --release
```

The resulting `target/wasm32-wasip2/release/myext_component.wasm` is the
distributable. Copy it into `artifacts/extensions/myext.wasm` (or any directory
the host scans).

## 4. Smoke test

```bash
# seed assertions from current output, review, then re-run to assert:
python3 tooling/smoke.py --seed-expected myext
python3 tooling/smoke.py myext

make ext-smoke-all        # smoke every extension
make ext-list-broken      # crates flagged un-buildable on wasm32-wasip2
python3 tooling/t-status.py   # tooling-improvement items from build experience
```

## 5. Load it through the host

The host runtime (`ducklink-host`) automatically looks in
`artifacts/extensions`. This is the **plugin** path — the extension stays a
separate `.wasm` and the core loads it at runtime; nothing is baked into the core:

```bash
cargo run -p ducklink-host --release -- \
  :memory: --load-extension myext -c "select myext_function();"
```

## 6. Or embed it into the core (`compose`)

The same extension can instead be **compiled into the core component** — it then
registers as a native scalar (no per-row WIT boundary, faster) and works in the
standalone with no host. Embedding is opt-in: by default the core embeds nothing
extra. Add an `embed-<name>` feature to the core crate (`dep:<crate>`; see
`embed-isin`), then select it with a flag (mirrors `sqlink compose`):

```bash
# list extensions that expose an embed-<name> core feature
ducklink compose --list

# build a core with isin embedded; optionally precompile to .cwasm
DUCKDB_STATIC_LIB=… DUCKDB_INCLUDE_DIR=… \
  ducklink compose --embed isin --output build/core-isin.wasm --precompile
```

`--embed a,b` maps to `cargo component build … --features wasi,embed-a,embed-b`.
The embed path doesn't disable the runtime loader — `LOAD`/`--load-extension`
still works for everything else. (`make core-embed EMBED=embed-isin` is the
equivalent Make target.)

## Tips

- Keep the sample extension handy as a reference for capability registration and
  runtime callbacks.
- If your extension requires assets (config files), package them alongside the
  `.wasm` and locate them via the WASI preopens in your `load()` logic.
- Most community extensions today rely on DuckDB's native C++ APIs instead of the
  component model. When porting one, focus on extensions whose surface is limited
  to scalar/table/aggregate functions so they fit the current API — or see
  [the community extensions reference](../reference/community-extensions.md) for
  the broader triage.
