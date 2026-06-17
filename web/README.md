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

## Run it

```bash
npm install
npm run dev      # vite dev server; open the URL and watch the <pre> fill in
npm run verify   # headless Chromium: instantiates + runs a query, prints result
```

`npm run verify` prints:

```
=== RESULT status: ok ===
columns: answer, two
rows: [[{"tag":"int64","val":"42n"},{"tag":"int64","val":"2n"}]]
```

i.e. `SELECT 42 AS answer, 1 + 1 AS two` ran inside headless Chromium — DuckDB
executing real SQL in the browser via wasi-polyfill, no native host. (Values
come back as WIT `duckvalue` variants; the core renders them as text.)

## Why the vite config looks the way it does

`vite.config.js` does two non-default things:

- `optimizeDeps.exclude` for `@tegmentum/wasi-polyfill` and
  `@bytecodealliance/jco` — jco's generated bindgen
  (`js-component-bindgen-component.js`) has a runtime-only `const offset = …;
  offset = …` reassignment that vite's esbuild pre-bundler rejects *statically*.
  Serving it as native ESM is fine (that branch isn't hit), so it must skip dep
  pre-bundling.
- `server.fs.strict: false` — jco loads its own `.core.wasm` helpers from the
  linked `wasi-polyfill` checkout, which is outside the project root.

(The same `blob:` import jco uses is why this runs in browsers but not Node's
ESM loader — `run-core.mjs` run directly under Node reaches transpilation then
stops on `import(blob:)`.)

## Extension loading in the browser (working)

`npm run verify-ext` loads the **sample extension component** into the in-browser
DuckDB and calls its registered functions — the full pipeline the native host
drives, in the browser:

```
=== RESULT status: ok ===
scalar      sample_plus_one(41)        = [[{"tag":"int64","val":"42n"}]]
macro       sample_add_two(40)         = [[{"tag":"int64","val":"42n"}]]
cast        id-7 -> sample_id          = [[{"tag":"int64","val":"7n"}]]
logical     7::sample_id               = [[{"tag":"int64","val":"7n"}]]
table       sample_emit_sequence(4)    = [[{"tag":"int64","val":"0n"}],...]
aggregate   sample_sum(1..4)           = [[{"tag":"int64","val":"10n"}]]
replacement FROM 'hello.sample'        = [[{"tag":"text","val":"hello.sample"}]]
```

That is every capability the sample extension registers — scalar / table /
aggregate / cast dispatch back to the loaded extension instance, while macro and
logical-type run as core SQL — all in the browser.

`extension-host.mjs` implements the host surface in JS:

- it instantiates the extension *component* (a second jco-transpiled component)
  with JS implementations of `duckdb:extension/{runtime,catalog,files}` — the
  registry/callback resource classes capture what `load()` registers;
- it provides the *core's* `duckdb:component/{host-extension-loader,
  extension-loader-hooks}` + `duckdb:extension/callback-dispatch` imports, so the
  core sees the captured registrations and dispatches scalar/table/aggregate/cast
  callbacks straight back to the extension instance.

Because component instantiation is async but the core calls `request-load`
synchronously, extensions are **pre-loaded**; `request-load` then returns the
cached result and callbacks dispatch synchronously.
