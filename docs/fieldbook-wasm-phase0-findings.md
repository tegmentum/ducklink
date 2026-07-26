# Fieldbook on wasm — Phase 0 findings

Purpose: de-risk the two Fieldbook-wasm transports before Phase 1
builds anything.

- **Product 1**: `fieldbook-cli.wasm` — a standalone `wasi:cli/run`
  component so users can `wasmtime fieldbook-cli.wasm mydata.duckdb`
  wherever wasmtime runs, without the native `ducklink` host binary.
- **Product 2**: an HTML notebook UI backed by DuckDB-in-the-browser
  and the fieldbook wasm engine — the "DuckDB Jupyter" demo page.

Sibling: [Phase 0 spike](./fieldbook-wasm-phase0-spike/) — throwaway
HTML + shell that proves Product 2's transport.

## 1. What already exists (baseline)

The Fieldbook stack the previous session shipped:

| Component | Location | Role |
| --- | --- | --- |
| `fieldbook-core` | `~/git/datalink/extensions/fieldbook-core/` | DB-agnostic `declare!` scalars + read-macro constants + backing-table DDL |
| `fieldbook-component` | `~/git/ducklink/extensions/fieldbook-component/` | Wasm shim -> `fieldbook.wasm` (~228 KB); imports `duckdb:extension/nested-exec@5.0.0` for its DDL/DML; SQL surface `fieldbook_create` / `fieldbook_add_entry` / `fieldbook_drop` / `fieldbook_record_run` + three read macros |
| `fieldbook-dotcmd` | `~/git/ducklink/extensions/fieldbook-dotcmd/` | Pluggable dot-command component (`.fb`, `.entry`, `.run`) — talks direct-SQL to the `__fieldbook_*` tables via `duckdb:dotcmd/spi`, so it works even without the engine loaded |
| Native `duckdb-fieldbook` | `~/git/duckdb-fieldbook/` | Rust CLI at `src/bin/fieldbook.rs` (862 LoC) + `src/orchestration.rs` (220 LoC). The in-process native extension shipped in 0.1.x is [deprecated](/Users/zacharywhitley/git/duckdb-fieldbook/DEPRECATED.md) — the wasm engine is the sole surface as of 0.2.0. |

Everything already ships. Fieldbook is a **wasm-first** stack today —
the native CLI is a thin orchestrator on top of the wasm engine.

## 2. Product 1 — `fieldbook-cli.wasm`

### 2.1 What the ducklink CLI actually is

`crates/ducklink-cli/src/lib.rs` **is** the CLI's REPL implementation
— a `wasi:cli/run` component in Rust, target `wasm32-wasip2`,
`crate-type = ["cdylib"]`
(`~/git/ducklink/crates/ducklink-cli/Cargo.toml:6-8`).

Building it (from `~/git/ducklink/`):

```sh
make standalone-cli
# -> target/wasm32-wasip2/release/ducklink_cli.wasm  (~340 KB, exists today)
```

The CLI component imports:
- `duckdb:component/database` — the DuckDB engine (supplied by
  `ducklink_core.wasm`).
- `duckdb:cli/dotcmd-host` — dot-command dispatch.
- All the standard `wasi:cli/*`, `wasi:filesystem/*`, `wasi:io/*`
  (verified via `wasm-tools component wit
  target/wasm32-wasip2/release/ducklink_cli.wasm` — 21-line world).

The engine itself (`ducklink_core.wasm`) lives in a sibling repo
`~/git/duckdb-wasm/`, built via `make core` from the ducklink Makefile.

### 2.2 The standalone wasm CLI already works — via `wac plug`

The proven recipe is in `scripts/smoke-cli.sh:44-49`:

```sh
# core needs the extension-loader host imports satisfied; wac-plug a stub.
wac plug ducklink_core.wasm --plug ducklink_loader.wasm \
    -o ducklink_core_loaded.wasm
# CLI needs the database import satisfied; wac-plug the loaded core.
wac plug ducklink_cli.wasm  --plug ducklink_core_loaded.wasm \
    -o ducklink_cli_standalone.wasm
# now runnable directly:
wasmtime run -W exceptions=y -C cache=y --dir . --dir "$ROOT/artifacts" \
    ducklink_cli_standalone.wasm -- mydata.duckdb -c "SELECT 1"
```

The `wac plug` composition yields a single `.wasm` whose only unresolved
imports are `wasi:*`, which any wasmtime binary satisfies. On this
machine `ducklink_loader.wasm` needs a WIT resync (the stub is pinned
to `duckdb:extension@4.0.0` while the core has moved to `@5.0.0` — a
`sync-stub-wit.sh` refresh); that is a routine v4-to-v5 catch-up chore,
not a transport blocker.

**Verdict**: Product 1 is essentially done today at the transport
level. The gap is fieldbook-specific packaging (below), not
"can we build a wasm CLI at all".

### 2.3 What Product 1 still needs (Phase 1)

The current `ducklink_loader.wasm` is a **no-op stub**
(`crates/ducklink-loader/src/lib.rs:32-45`: `request_load -> false`,
`get_pending_registrations -> empty`). A standalone composed wasm
therefore cannot load `fieldbook.wasm` at runtime — even if the file
sits next to it in a preopen.

Three ways to fix this for `fieldbook-cli.wasm`:

1. **Embed via wac** (recommended, mirrors mosaic's shipped-bundle
   pattern) — wac-plug `fieldbook.wasm` and `fieldbook-dotcmd.wasm` into
   the standalone CLI so the fieldbook surface is available on startup
   with no --load flag. The loader stub becomes "no dynamic extensions
   loadable, but the fieldbook one is baked in" — a real (not stubbed)
   loader that answers `request_load("fieldbook")` from a compile-time
   map. Cost: one small Rust crate.
2. **Runtime-load from preopen** — teach the loader-stub to read
   `.wasm` files from a wasi preopen (e.g. `/extensions/`) and
   instantiate them. Cost: much higher — the loader stub becomes a
   real wasmtime-in-wasm, which isn't a thing today. **Rejected**.
3. **Ship the extensions alongside** with a docs recipe telling the
   user to pass `--dir extensions --load-extension fieldbook`. Cost:
   near-zero, but fails the "single-file distributable" bar the task
   asks for. **Reject.**

**Recommendation**: Option 1. It packages Fieldbook the same way a
distributable native binary would package its features, produces one
`.wasm` the user can just `wasmtime` at, and reuses the shipped
mosaic-Phase-1 embed pattern verbatim.

### 2.4 Cross-referenced artifacts

- CLI source: `/Users/zacharywhitley/git/ducklink/crates/ducklink-cli/src/lib.rs`
- Compose recipe: `/Users/zacharywhitley/git/ducklink/scripts/smoke-cli.sh:44-49`
- Loader-stub source: `/Users/zacharywhitley/git/ducklink/crates/ducklink-loader/src/lib.rs`
- Fieldbook shim: `/Users/zacharywhitley/git/ducklink/extensions/fieldbook-component/src/lib.rs`
- Fieldbook dot-command: `/Users/zacharywhitley/git/ducklink/extensions/fieldbook-dotcmd/src/lib.rs`

## 3. Product 2 — browser Fieldbook

### 3.1 Two DuckDB-in-browser stacks — they are NOT interchangeable

| Stack | Format | Extension model | Fieldbook fit |
| --- | --- | --- | --- |
| **`@duckdb/duckdb-wasm`** (npm; duckdb-labs) | Emscripten build of DuckDB C++; loaded via a JS worker + `.wasm` blob from a CDN | Loads C++ `.duckdb_extension` shared objects from a curated URL list (`duckdb-wasm-extensions/*` on CDN); no support for wasm-component extensions | **Cannot load `fieldbook.wasm`** — the loader wants Mach-O/ELF, not a component |
| **`~/git/duckdb-wasm/`** (ducklink team's core, `duckdb-component-core`) | DuckDB C++ compiled to `wasm32-wasip2` **as a WIT component**; the exact same artifact the native `ducklink` binary embeds via wasmtime | Loads `duckdb:extension` WIT components (all 68+ shipped extensions in `artifacts/extensions/`, including `fieldbook.wasm`) | **This is the one that works.** Browser hosting via `@bytecodealliance/jco` transpile + `@tegmentum/wasi-polyfill` — already wired in `~/git/ducklink/web/` |

The confusion comes from the same package-space suggesting they're
related. They are not — different codebases, different ABIs, different
extension formats.

### 3.2 The `~/git/ducklink/web/` browser prior art

The ducklink team already built an in-browser DuckDB extension loader
that runs verbatim what we need:

- `web/run-core.mjs:160-192` — `instantiateCore(componentBytes,
  additionalImports)` transpiles `ducklink_core.wasm` with jco (JSPI
  mode) and wires WASI imports via `@tegmentum/wasi-polyfill`.
- `web/extension-host.mjs:13-77` — `createExtensionHost()` pre-loads
  extension components, captures their registrations, and feeds the
  callback dispatch back into the core.
- `web/browser-ext-entry.mjs:22-56` — end-to-end demo: loads
  `ducklink_core.wasm` + `sample_extension.wasm`, executes
  `LOAD sample_extension`, then runs scalar / macro / cast / logical /
  table / aggregate / replacement-scan queries — every capability
  registers and dispatches correctly.

`fieldbook.wasm` is a `duckdb:extension@5.0.0` component using the
same runtime SPI as `sample_extension.wasm` (scalar registrations +
one nested-exec import for its bootstrap DDL). If the sample works,
fieldbook works — the only interface not exercised by the sample is
`duckdb:extension/nested-exec@5.0.0`, which the polyfill glue would
need a JS implementation of. That JS side is 20 lines: `nested_exec(sql)
-> call db.execute(sharedConn, sql)`. Nothing exotic.

### 3.3 What the spike proved (and did not)

The Phase 0 spike is `docs/fieldbook-wasm-phase0-spike/spike.html` +
`run.sh`. Deliberately uses `@duckdb/duckdb-wasm` from the CDN — the
"generic in-browser DuckDB" story users first reach for — so the two
questions get concrete answers on one page.

Static-serve smoke confirmed:

```
sanity: fetch spike.html       -> 200
sanity: fetch fieldbook.wasm   -> 200  233987 bytes
sanity: fetch nonexistent      -> 404
```

The three jsdelivr URLs the page loads are live:

```
esm:       200  (@duckdb/duckdb-wasm@1.29.0/+esm)
mvp wasm:  200  (dist/duckdb-mvp.wasm)
eh worker: 200  (dist/duckdb-browser-eh.worker.js)
```

I did not open the page in a real browser under this run — that's a
30-second interactive step for the user. The expected outcome is
documented directly on the page: (i) the vanilla `SELECT * FROM demo`
table renders (20 rows, `x` and `y=x*x`), and (ii) the fieldbook.wasm
load pane shows the expected rejection with the wasm-component bytes
being fed to a Mach-O-/ELF-expecting loader.

If (i) fails the transport story for the mainline stack is broken —
which would be surprising, `@duckdb/duckdb-wasm` is widely used.
If (ii) succeeds against expectations, that would be a genuine
surprise and would call for a rethink.

**What the spike does not prove**: that `~/git/ducklink/web/` +
`fieldbook.wasm` works browser-side. It doesn't need to — the sibling
`browser-ext-entry.mjs` already proves the pattern for a
same-shaped extension (sample_extension.wasm). Wiring fieldbook there
is Phase 1 work, not a Phase 0 unknown.

### 3.4 Persistence — `.duckdb` file portability

The `.duckdb` file format is byte-compatible across every DuckDB build
at the same version — this is the whole point of the format. The
matrix that matters:

- **Browser -> native**: the browser writes `.duckdb` into
  `wasi:filesystem/preopens` (the polyfill's memory FS today). To
  export, the JS layer reads the bytes and offers a download; the user
  opens it in the native `fieldbook` CLI or `ducklink`.
- **Native -> browser**: the user drops a `.duckdb` on the page; JS
  writes it to the polyfill FS with `preopens.write` before opening.
- **OPFS** (Origin Private File System) is the modern browser
  persistent-file story. `wasi-polyfill` currently uses in-memory FS
  (`web/run-core.mjs:44-49`); an OPFS-backed
  `wasi:filesystem/preopens` implementation exists in the ecosystem
  (`@bjorn3/browser_wasi_shim` and others) but is not wired here yet.

**Phase 1 recommendation**: ship the browser demo with in-memory FS +
a "download `.duckdb`" button. OPFS integration is a Phase 2
enhancement — it's a solved problem in the polyfill layer, not a
Fieldbook question.

### 3.5 UI framework recommendation

The task calls for a "Jupyter notebook, but for DuckDB": cells
containing SQL, run buttons, output cells rendering table results.

Candidate stacks:

- **Vanilla JS + Observable Plot** (matches the mosaic Phase 0 spike).
  Suffices for the smoke; awkward as soon as you want cell reordering,
  cell IDs, or a scrollable dashboard.
- **Solid.js** (the mosaic Phase 1 embed reaches for this-ish patterns;
  small runtime, fine-grained reactivity). ~20 KB gz.
- **React** (biggest ecosystem, but ~40 KB gz baseline).
- **Lit web components** (~5 KB gz).

**Recommendation**: **Lit** for Phase 1.
- Notebook cells are naturally web components: `<fb-cell>` with slots
  for source + output. Reordering, IDs, deletion all fall out of the
  DOM.
- No JSX / TSX toolchain — fits the mosaic esbuild pattern verbatim
  (`extensions/mosaic-component/browser/esbuild.config.mjs`).
- Familiar to anyone who's touched observable notebooks or Jupyter's
  cell model, no surprises for the fieldbook orchestration semantics
  (`.run` executes each cell in order).

Rendering the query result cell: an HTML table for the Phase 1 demo,
mirroring what `duckdb-fieldbook`'s native CLI prints via
`comfy-table`. Observable Plot as an optional per-cell "chart" toggle
is Phase 2 — adds a viz cell without changing the core model.

**Mosaic composition is NOT recommended for Phase 1.** Mosaic is a
dashboard model (many linked views over one SQL space); a notebook is
an ordered sequence of independent queries. Different problem.

### 3.6 Bundle build

Reuse the mosaic Phase 1 pattern verbatim
(`extensions/mosaic-component/browser/`):
- `entry.js` — imports Lit + the runtime glue (jco loader +
  wasi-polyfill), assigns to `window.fieldbookRuntime`.
- `esbuild.config.mjs` — IIFE bundle to `dist/bundle.js`, staged
  `dist/index.html` shell.
- `dist/*` committed to git so the wasm build reads it via
  `include_bytes!` — same as mosaic.
- `make fieldbook-browser` target that runs `npm ci && npm run build`.

## 4. Cross-cutting

### 4.1 SQL surface parity

The four scalar functions (`fieldbook_create`, `fieldbook_add_entry`,
`fieldbook_drop`, `fieldbook_record_run`) and three read macros
(`fieldbook_list`, `fieldbook_entries`, `fieldbook_source`) live in
`fieldbook-core` and are wired identically by:

- `fieldbook-component` for the wasm engine (used in the CLI, browser,
  and native ducklink extension paths).
- `duckdb-fieldbook`'s deprecated 0.1.x native extension.

There is one SQL surface. Wherever `nested-exec` works, the surface
works. Under the standalone wasm CLI this hits the trap noted in
`docs/mosaic-phase0-findings.md` (nested-exec re-entry blocked pending
an ecosystem-wide core rebuild) — same blocker mosaic hit,
same-shaped workaround if Phase 1 needs it.

### 4.2 Dot-command surface parity

`fieldbook-dotcmd` bypasses the engine scalars and writes DDL/DML
direct to `__fieldbook_*` tables via the `duckdb:dotcmd/spi`
(`extensions/fieldbook-dotcmd/src/lib.rs:11-25`), so
`.fb new / .entry add / .run` work in the standalone CLI **today**
without waiting on the nested-exec re-entry fix. This is why Product 1
is closer to done than Product 2 — the CLI can drive Fieldbook via dot
commands only, avoiding the engine trap.

The browser demo, in contrast, doesn't have the dotcmd host running.
It calls the engine scalars directly, so it will hit the nested-exec
trap the moment `fieldbook_create` is invoked — until the same fix
mosaic is waiting on lands.

## 5. Top 5 findings that shape Phase 1

1. **Product 1's transport is done.** `wac plug` composes
   `ducklink_cli.wasm` + `ducklink_core.wasm` + a loader stub into a
   standalone runnable component today
   (`scripts/smoke-cli.sh:44-49`). Fieldbook-specific work reduces to
   "bake `fieldbook.wasm` + `fieldbook-dotcmd.wasm` into that compose
   and teach the stubbed loader to return them on request-load".
2. **The mainline `@duckdb/duckdb-wasm` cannot host our fieldbook.**
   It's an Emscripten build with a C++ extension loader. Product 2
   goes through `~/git/duckdb-wasm/` + `~/git/ducklink/web/` — the
   jco + wasi-polyfill browser stack that already runs extension
   components in `web/browser-ext-entry.mjs`.
3. **The nested-exec trap is a shared blocker with mosaic.** Any
   invocation of `fieldbook_create` from either the standalone CLI or
   the browser hits the same "cannot enter component instance" trap
   documented in `docs/mosaic-phase0-findings.md`. The dot-command
   surface sidesteps this in the CLI path (writes direct SQL via
   `spi`); the browser path has no equivalent side door.
4. **Notebook UI = Lit web components + esbuild.** Cell = web
   component; reordering / deletion is DOM. Reuse the mosaic browser
   build harness verbatim. Rendering is an HTML table for Phase 1;
   Observable Plot as an optional Phase 2 chart cell.
5. **Persistence is solved-at-the-format-layer.** `.duckdb` files are
   byte-portable native <-> browser at the same DuckDB version. The
   Phase 1 browser demo uses in-memory FS with a "download `.duckdb`"
   button; OPFS is a Phase 2 polyfill config change, not a Fieldbook
   design decision.

## 6. Phase 1 build plan

Sizes: **S** ~half-day, **M** 1-2 days, **L** 3+ days.

### 6.1 Product 1 — `fieldbook-cli.wasm`

1. **(S)** Sync `crates/ducklink-loader/wit/deps/duckdb-extension` from
   v4 to v5 via `scripts/sync-stub-wit.sh`; unblock `make loader-stub`
   so the existing standalone recipe builds green.
2. **(S)** New crate `crates/fieldbook-loader/`. Same shape as
   `ducklink-loader`, but its `request_load(name)` returns `true` for
   `"fieldbook"` and instantiates the baked-in `fieldbook.wasm` (via
   `include_bytes!`), draining its registrations through the standard
   `extension-loader-hooks` contract.
3. **(S)** Compose recipe in `scripts/build-fieldbook-cli.sh`:
   ```sh
   wac plug ducklink_core.wasm --plug fieldbook_loader.wasm \
       -o core_fb.wasm
   wac plug ducklink_cli.wasm  --plug core_fb.wasm \
       --plug fieldbook_dotcmd.wasm \
       -o fieldbook_cli.wasm
   ```
   Output: `artifacts/fieldbook-cli.wasm` (~5-8 MB compressed).
4. **(S)** Auto-load on startup: `crates/fieldbook-cli/`, a copy of
   `crates/ducklink-cli/` that emits `LOAD fieldbook;` before the REPL
   prompt (or the `-c` script) runs. Alternate: leave the CLI generic
   and rely on the loader's `preload_extensions` mechanism.
5. **(M)** `make fieldbook-cli` Makefile target + a `smoke.sql` +
   `run.sh` that runs the fieldbook standalone against a scratch
   `.duckdb` and prints "1 book, 1 entry, 1 successful run".
6. **(S)** Docs — user runs
   `wasmtime run -W exceptions=y -C cache=y --dir . fieldbook-cli.wasm -- mydata.duckdb`
   and gets an identical experience to native `fieldbook`.

**Constraint dependency**: the nested-exec trap. Until it lifts, the
CLI experience is dot-commands only (fine — that's the whole
authoring surface). Engine scalars still trap under the standalone
compose the way they do for mosaic — user-visible if they type
`SELECT fieldbook_create(...)` at the prompt.

### 6.2 Product 2 — Browser Fieldbook

1. **(M)** New extension crate `extensions/fieldbook-browser/`. Not a
   duckdb:extension — a plain browser bundle + a small companion
   Rust crate that composes `ducklink_core.wasm` + `fieldbook.wasm`
   into a browser-target artifact.
2. **(M)** Notebook UI in `extensions/fieldbook-browser/browser/`
   (Lit + esbuild + mosaic-Phase-1 pattern):
   - `<fb-notebook>` root — holds ordered `<fb-cell>` children; `.run`
     button, `.export .duckdb` button.
   - `<fb-cell>` — CodeMirror-lite or `<textarea>` source, run button,
     output pane (HTML table).
   - `runtime.js` — the jco+polyfill loader (adapted from
     `web/run-core.mjs`) with a JS `nested-exec` implementation.
3. **(M)** `.duckdb` file drop-in / export via
   `wasi:filesystem/preopens` reads/writes.
4. **(S)** `make fieldbook-browser` target — npm ci + esbuild build;
   commits `browser/dist/{index.html,bundle.js}` to git as mosaic does.
5. **(S)** Demo page `docs/fieldbook-browser-demo/index.html` served
   from any static host. Screenshot in Phase 1 PR body.
6. **(S)** Persistence UX: "download `.duckdb`" + "load `.duckdb`"
   affordances; OPFS deferred.

**Blocker on nested-exec**: `fieldbook_create` traps until the
ecosystem fix ships. Workaround identical to mosaic's
`mosaic_install_sql` pattern: the extension exposes a pure
`fieldbook_bootstrap_sql()` that returns the DDL text; the browser
runs it top-level. Phase 1 UI hides this behind the "New fieldbook"
button.

## 7. Open questions for the user before Phase 1

1. **Distribution shape for `fieldbook-cli.wasm`**: single embedded
   `.wasm` (Option 1 in §2.3) — confirm, or should we ship the CLI +
   extensions as a directory tree instead? Recommendation: single
   `.wasm`.
2. **Notebook UI framework**: Lit (§3.5) or something else you want?
   Options considered: vanilla JS, Solid, React, Lit; recommended
   Lit.
3. **Nested-exec workaround policy**: mirror mosaic's
   `_install_sql`-style pure-return pattern in fieldbook now (so
   Phase 1 users don't hit the trap), or wait for the ecosystem
   rebuild? Recommendation: mirror; the fix is small and self-contained
   in `fieldbook-core`.
4. **Persistence**: in-memory + download button for Phase 1, OPFS in
   Phase 2 — confirm the sequencing.
5. **Browser DuckDB stack**: use ducklink's `~/git/duckdb-wasm/` core
   (which supports our WIT extensions) rather than
   `@duckdb/duckdb-wasm`. Confirm this is the right call — the
   trade-off is bundle size (~5-10 MB gz for the componentized core
   vs. ~2 MB gz for `@duckdb/duckdb-wasm`) in exchange for
   fieldbook actually working.
6. **Should Product 1 also drop the native `duckdb-fieldbook` CLI**
   (deprecating it in favor of `fieldbook-cli.wasm`)? Or run both in
   parallel with the native one wrapping the wasm one? Not a Phase 0
   question but worth answering before we commit the packaging.
