# fieldbook browser demo

Product 2 of the Fieldbook-wasm initiative — the "DuckDB Jupyter" demo
page. A pure browser artifact that:

- Loads the **WIT-based ducklink DuckDB core** from
  `~/git/duckdb-wasm/target/wasm32-wasip2/release/ducklink_core.wasm` via
  [`@bytecodealliance/jco`][jco] + [`@tegmentum/wasi-polyfill`][polyfill].
- Loads (Phase 1: **ships** but does **not** activate) `fieldbook.wasm` —
  the wasm-component engine at
  `~/git/ducklink/artifacts/extensions/fieldbook.wasm`.
- Mounts a [Lit][lit] web-component notebook UI: SQL cells, run buttons,
  HTML-table results, download-.duckdb button. Same UX shape as a Jupyter
  notebook, but for DuckDB.

> **NOT** the npm `@duckdb/duckdb-wasm` — that's an Emscripten build with
> a C++ extension loader that cannot host our WIT components (see
> `docs/fieldbook-wasm-phase0-findings.md` §3.1 for the compare-and-contrast
> and `docs/fieldbook-wasm-phase0-spike/spike.html` for the demonstration).

## Build

From the repo root (`~/git/ducklink`):

```sh
make fieldbook-browser
```

That runs `npm install` in `web/fieldbook/`, `vite build` from
`src/index.html`, and copies the two wasm artifacts into `dist/`. Output:

```
web/fieldbook/dist/
  index.html                                              # SPA shell
  assets/main-<hash>.js                                   # ~190 KB — app entry
  assets/browser-<hash>.js                                # ~220 KB — wasi-polyfill
  assets/browser-<hash>.js                                # ~1 KB — vite chunk
  assets/js-component-bindgen-component.core-<hash>.wasm  # ~9.2 MB — jco transpiler
  assets/js-component-bindgen-component.core2-<hash>.wasm # ~16 KB — jco transpiler
  ducklink_core.wasm                                      # ~45 MB — WIT DuckDB core
  fieldbook.wasm                                          # ~230 KB — shipped for Phase 2
```

Unlike the mosaic-browser sibling, `dist/` is NOT committed to git —
between the ~9 MB jco transpiler wasm, the ~45 MB DuckDB core, and the
hashed asset filenames Vite produces, in-tree distribution isn't
practical. `make fieldbook-browser` regenerates everything from source
in ~10 s once `node_modules/` is warm.

### Why Vite (not esbuild)

The mosaic sibling uses esbuild directly; we can't. `@tegmentum/wasi-polyfill`'s
`createRuntimeBindgen` dynamically imports `@bytecodealliance/jco/component`
at runtime for wasm-component transpilation. jco's bundle contains
(a) a const-reassignment that trips esbuild's strict parser, and
(b) `await import('node:fs/promises')` in browser-unreachable helpers that
esbuild still tries to resolve at bundle time. Vite handles both — the
dynamic import is preserved as a lazy chunk, node-only branches are
transparently shimmed with `__vite-browser-external`. The polyfill's own
e2e tests use Vite for the same reason (see
`~/git/wasi-polyfill/test/e2e/vite.config.ts`).

## Run

```sh
bash web/fieldbook/run.sh          # default port 8789
PORT=9000 bash web/fieldbook/run.sh
```

Opens on `http://127.0.0.1:8789/` — needs **Chrome 137+** (the WASI JSPI
bootstrap in `db.js` promotes wasi:io/poll to a suspending import; JSPI
is what lets DuckDB's `execute` yield the event loop). Same requirement
as the sibling `web/browser-ext-entry.mjs` demo.

## What you see

1. Top bar with **+ New cell**, **Run all**, **Download .duckdb**.
2. Two starter cells:
   - `SELECT 42 AS answer` — scalar shape
   - `SELECT range AS n, (range * range)::BIGINT AS n_squared FROM range(1, 6)` — tabular shape
3. Each cell: textarea SQL + Run button + status line + output pane
   (HTML table for row-producing statements, error text on failure,
   "(no rows)" for empty results).
4. Cmd/Ctrl-Enter runs the focused cell.
5. **Download .duckdb** flushes with `CHECKPOINT` and downloads the
   current database as `fieldbook.duckdb`. Open in the native `fieldbook`
   CLI (`~/git/duckdb-fieldbook/`) or `ducklink` — schema is
   byte-identical to what those write.

## Architecture

```
index.html
  ↓ loads
bundle.js (IIFE)
  ↓ boot()
db.js         — instantiateCore(coreWasmBytes) via jco+polyfill
  ↓ (db, conn)
notebook.js   — <fieldbook-notebook>, holds ordered cell array
  ↓ per-cell
cell.js       — <fieldbook-cell>, source textarea + Run + output
  ↓ dispatches
fieldbook-api.js — direct SQL against __fieldbook_{books,entries,runs,state}
  ↓
ducklink_core.wasm (over jco + wasi-polyfill preopens=/)
```

### Why direct SQL, not fieldbook.wasm engine scalars

`fieldbook.wasm` exposes `fieldbook_create` / `fieldbook_add_entry` /
`fieldbook_drop` scalars that internally use the
`duckdb:extension/nested-exec` host import to run DDL/DML. In the
single-process wasm-DuckDB world (native ducklink and this browser demo
share the shape) that nested-exec path hits a re-entry trap — the same
blocker mosaic is waiting on. The `fieldbook-dotcmd` component
sidesteps it by going direct-SQL against the `__fieldbook_*` tables
(`extensions/fieldbook-dotcmd/src/lib.rs::ensure_bootstrap`); this
demo does the same. Schema is byte-identical to what the engine writes,
so the two worlds share `.duckdb` files freely.

`fieldbook.wasm` is still copied into `dist/` so a Phase 2 upgrade can
wire the engine in (once nested-exec re-entry lands) without changing
the URL layout.

## Phase 2 gaps

- **OPFS persistence.** Currently in-memory (with a download button).
  `wasi-polyfill` already ships an OPFS filesystem implementation
  (see `~/git/wasi-polyfill/dist/wasip2/plugins/filesystem`); wiring
  is a policy-config change in `db.js::configurePolyfill`.
- **Chart cells.** Phase 1 renders every result as an HTML table.
  Observable Plot per-cell would slot into `<fieldbook-cell>`
  as an alternate output mode.
- **Load fieldbook.wasm.** Bootstrap DDL is duplicated in
  `fieldbook-api.js::DDL` and `fieldbook-core::CREATE_BOOKS`; once
  nested-exec re-entry works we can just call
  `LOAD fieldbook; SELECT fieldbook_create('demo')` at boot.
- **File drag-and-drop / upload.** Round-tripping a `.duckdb` from
  the native CLI back into the browser needs a `<input type=file>`
  affordance that writes bytes into the polyfill's memfs pre-instantiation
  via `MemoryFileSystem.fromEntries` + `setGlobalFilesystem`.

[jco]: https://github.com/bytecodealliance/jco
[polyfill]: https://github.com/tegmentum/wasi-polyfill
[lit]: https://lit.dev/
