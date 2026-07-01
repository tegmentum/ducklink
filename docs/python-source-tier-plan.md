# ducklink Python Source Tier — Plan

Status: **designed 2026-07-01, not built.** All key decisions settled.

## Goal

Author a ducklink extension in **Python** and run it with **no compilation**.
Guiding principle: *"you should be able to compile it — you just shouldn't have
to."* This is a third authoring tier alongside the compiled ones.

| Tier | Build | Speed | Size | Audience |
|---|---|---|---|---|
| Rust/C → wasm | toolchain | fastest | small | perf-critical |
| Python → compiled | opt-in | fast | ~20 MB (embeds CPython) | ship a Python ext standalone |
| **Python → interpreted** | **none** | slower | source-only | glue, prototyping, reach |

One authoring API; **two execution modes off the same `.py`** — **run** (interpret,
zero-build, default) and **compile** (opt-in). Prototype interpreted, graduate to
compiled when a hot path or a shippable artifact demands it.

## Architecture: drive pylon, don't build a runtime

ducklink does **not** build a Python runtime — it drives **pylon**
(`~/git/python-wasm`): CPython 3.14 (+ MicroPython 1.24) compiled to
`wasm32-wasip2`, **dynlink-first** (ADR-001), on the *same* compose:dynlink
framework as the retired sqlink loader. Pylon already provides the two hard
pieces:

- **`tegmentum:py-offload`** — `offload.run(env-id, task{entry:"module:callable",
  args:list<u8>, codec}) -> outcome(ok(bytes)|raised(py-error))`. That *is* "call
  a Python function with encoded args, get an encoded result."
- **`py-package` + composectl** — content-addressed `env-id` (interpreter +
  resolved wheels), backend selection, and composectl composing a
  content-addressed **worker artifact** = the "compile" mode, already built.
- **deps** — pip into a `$PYLON_HOME/site-packages` mount today; uv-wasm as its
  own component on pylon's roadmap.

ducklink hosts pylon as a **resident, reentrant dynlink provider** (the #227/#228
machinery — warm, stateful, reentrant).

## Settled decisions

1. **Discovery = manifest via `offload.run`.** The ducklink Python API builds a
   manifest inside the script's module; ducklink reads it via
   `offload.run(entry="ducklink.runtime:manifest")`. **Zero pylon change**; pylon
   stays generic.
2. **Codec = arrow-columnar from day one.** A column of N rows per `offload.run`,
   aligned with the ducklink `@4.0.0` columnar ABI. **Requires implementing
   pylon's reserved `arrow` codec** + columnar marshalling — the one pylon-side
   build item.
3. **Default runtime = CPython 3.14.** MicroPython is opt-in later.

## What ducklink adds (small)

1. **ducklink Python API** (pure-Python, shipped into the script env):
   `@ducklink.scalar/table/aggregate` decorators mapping type hints → WIT/DuckDB
   types; builds the manifest; exposes the `ducklink.runtime:manifest` entry.
2. **`ducklink_run('x.py')` glue**: host pylon as a resident reentrant provider;
   run script → read manifest → register SQL functions → dispatch each call via
   `offload.run` (arrow-columnar, batched).
3. **pylon arrow codec** (the one pylon-side piece).
4. **PEP 723 inline deps** (`# /// script`) → resolved to a pylon `env-id`.

## Dispatch flow

`ducklink_run('x.py')`:
1. Resolve the script's inline deps → a pylon `env-id`.
2. Instantiate/reuse the resident pylon provider (warm).
3. `offload.run(entry="x")` — load the script; the decorators populate the manifest.
4. `offload.run(entry="ducklink.runtime:manifest")` → `[{name, kind, args, returns, entry}]`.
5. Register each as a SQL scalar/table/aggregate.

Per SQL call (batched, columnar):
6. Encode the input **column** (arrow) → `offload.run(task{entry:"x:fn",
   args:<arrow column>, codec:arrow})` → decode the output column.

Reentrancy: a Python function that itself runs SQL re-enters the engine via the
proven #221 deep-reentrant provider path.

## Two modes, same `.py`

- **run** — drive a shared resident pylon worker; zero-build; default.
- **compile** — composectl emits a content-addressed worker with the script baked
  in (pylon already does this); for speed / self-contained distribution / no
  pylon dependency. `py-package.tier: in-wasm`.

## Phasing

- **Phase 0 — spike:** one scalar end-to-end (`@ducklink.scalar def upper(s:str)->str`),
  **msgpack-per-row** for speed of proof, on the wasmtime host — validates
  run → manifest → register → dispatch before the arrow investment.
- **Phase 1:** the ducklink Python API (scalar/table/aggregate + manifest) + the
  `ducklink_run` resident-provider glue.
- **Phase 2:** pylon **arrow codec** + arrow-columnar batched dispatch (the perf
  tier, `@4.0.0`-aligned).
- **Phase 3:** PEP 723 inline deps → `env-id` resolution.
- **Phase 4:** **compile mode** (composectl worker artifact) + catalog integration
  (source modules in the catalog).
- **Phase 5:** MicroPython opt-in; native-dep tier (Pyodide-style wasm wheels).

## Open items / risks

- **pylon arrow codec is unimplemented** (Phase 3 on pylon's own roadmap) — the
  main new pylon-side work the codec decision forces.
- **Native deps** (numpy/pandas) need pre-built wasm wheels — a later tier; pure-
  Python deps are tractable now.
- **Perf:** even batched, interpreted Python is slower than compiled — this tier
  is for reach/convenience; the compiled tier owns hot paths.
- **Browser:** pylon runs in-browser via jco; the deep-reentrant arm needs jco
  1.15.x (per the #228 caveat).
