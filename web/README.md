# DuckDB component in the browser via wasi-polyfill

Runs the same `duckdb_core_component.wasm` (wasip2 component) that the native
`duckdb-host` runs, but in a JS runtime / browser — WASI is provided by
[`@tegmentum/wasi-polyfill`](https://github.com/.../wasi-polyfill) and the
component is transpiled with [jco](https://github.com/bytecodealliance/jco).

## What's here

- `run-core.mjs` — `configurePolyfill()` registers every WASI plugin the core
  imports; `runQuery(bytes, sql)` instantiates the core via the polyfill's
  `RuntimeBindgen` (jco transpile + import wiring) and runs a query. The custom
  `duckdb:*` host imports (extension loader hooks, callback dispatch) are stubbed
  — plain queries don't invoke them.
- `browser-entry.mjs` + `index.html` — fetch the core wasm and show the result.

```bash
npm install
# copy the core wasm next to index.html so the page can fetch it:
cp ../target/wasm32-wasip2/release/duckdb_core_component.wasm .
```

## Verified so far

Running `node run-core.mjs` (or stepping through `runQuery`) confirms:

- **all 21 WASI interfaces** the component imports (`wasi:cli/*`, `wasi:io/*`,
  `wasi:filesystem/*`, `wasi:clocks/*`, `wasi:random/*`, including the
  `terminal-*` and `insecure-*` ones) resolve through the polyfill's plugins;
- jco transpiles the ~40 MB component to core modules + JS;
- instantiation proceeds through import binding.

## Two known obstacles (both environment/packaging, not the component)

1. **Node**: `RuntimeBindgen` imports the jco-generated module from a `blob:`
   URL, which Node's ESM loader rejects (`ERR_UNSUPPORTED_ESM_URL_SCHEME`).
   `blob:` import works in browsers, so this is a Node-only limitation.
2. **Bundling**: esbuild cannot bundle jco's browser build —
   `@bytecodealliance/jco/obj/js-component-bindgen-component.js` contains a
   `const offset = …; offset = …` reassignment that esbuild flags statically.
   (It would only throw at runtime if that path executes, so loading jco as
   native ESM is fine; only ahead-of-time bundling trips on it.)

## Next step

Serve `index.html` with **native ESM + an import map** (or a dev server that
serves source rather than fully bundling jco) so the page loads
`@tegmentum/wasi-polyfill` and jco unbundled, then open it / drive it with a
headless browser to confirm `SELECT 42` returns in-browser. Once a plain query
runs, wiring the custom `duckdb:*` imports to real implementations brings
extension loading to the browser too.
