# PLAN: Python ML functions sharing one composed pylon

## Goal

Add a family of DuckDB functions backed by Python ML (numpy — "scikit-learn-like",
see scope) running on **pylon** (`~/git/python-wasm`, CPython 3.14 / MicroPython
compiled to `wasm32-wasip2`), and — the real point — wire them so **one copy of
pylon backs many functions** instead of each function embedding its own. This is
the most compelling demonstration of the WIT-composition thesis: a heavy
dependency amortized across many tiny components.

## Why this is worth doing

- A single pylon ML component is **~38 MB** (CPython + numpy; the existing
  `pylon-kmeans-numpy.component.wasm` is 38 MB). Embedding pylon per function ⇒
  38 MB × N. Sharing one ⇒ 38 MB once + tiny function components. That delta is
  the composition value proposition, made concrete — a more dramatic version of
  the postgis/mobilitydb shared-proj/geos composition we already shipped.
- The ML piece is already de-risked: `pylon-kmeans-numpy` exists (k-means via
  numpy is a canonical sklearn algorithm). numpy is a real wasm package in pylon's
  catalog (`packages-wasm/numpy`), alongside msgpack/cbor2/ujson codecs.

## Background — what already exists (don't rebuild)

- **pylon** runs Python as a `wasm32-wasip2` component (wasmtime native + jco
  browser), CPython 3.14 default.
- **The offload contract** `tegmentum:py-offload@0.1.0` (`~/git/python-wasm/wit/
  py-offload.wit`) is the call seam:
  - `world py-worker { export offload; }` — the shareable worker.
  - `run: func(env: env-id, t: task) -> outcome`.
  - `task { entry: string, args: list<u8>, codec: codec }` — `entry` is
    `"pkg.module:callable"`; `args` is `{args:[...], kwargs:{...}}` encoded with
    `codec ∈ {json, msgpack, arrow, pickle}`.
  - `outcome = ok(list<u8>) | raised(py-error{kind,message,traceback})` — Python
    exceptions cross as structured errors, not traps.
  - `env-id = string` — an opaque, persistent environment (hold a fitted model
    across calls; content-addressed in pylon's full design).
- The `reference-worker` is the canonical offload implementation; `pool.py` shows
  host-side fan-out across resident workers (a different sharing tier).

## The offload seam is the whole interface

A ducklink Python-ML function component needs to do exactly one thing across the
boundary: `offload.run(env, task)`. Everything else (which algorithm, fit vs
predict) is encoded in `task.entry` + `task.args`. So the ducklink side stays thin
and generic; the ML lives in Python on pylon.

## Design fork — how "one copy" is achieved (the key decision)

| | **wac pre-composition** | **host-mediated (recommended)** |
|---|---|---|
| Mechanic | bundle N function components + 1 pylon `py-worker` into one composed artifact via `wac plug` | the ducklink host instantiates **one** pylon `py-worker` and supplies the `py-offload` import to every dynamically-loaded function component |
| Fit to ducklink | static — the function set is fixed at compose time | **dynamic — matches how ducklink already loads components and injects host imports (storage/files/query)** |
| "One copy"? | yes (one pylon in the bundle) | yes (one resident pylon instance serves all) |
| Best for | a sealed, shippable ML bundle | the live, extensible catalog |

**Recommendation:** validate the **wac** mechanic first (it's the cheapest proof
that many importers can share one `py-worker`), then land **host-mediated** sharing
as the production model. Both satisfy "one copy of pylon for multiple functions";
host-mediated fits the dynamic catalog.

### Host-mediated sharing via `compose:dynlink` (the production layer)

Realize host-mediated sharing on the **`sys:compose@1.0.0` orchestration
framework** (`~/git/webassembly-component-orchestration`), specifically
`compose:dynlink@0.1.0` — a dependency-injection layer for Wasm components
("Guice for Wasm") that **sqlink's host already runs on**
(`sqlink/host/src/compose_provider.rs`). The mapping is exact:

- **pylon = a `dynlink-provider`** — it provides the `tegmentum:py-offload/offload`
  endpoint.
- **each ML function component = a `dynlink-guest`** — it imports `compose:dynlink/
  linker` to resolve and call the shared provider.
- The ducklink host holds **one** resident pylon provider and serves every guest —
  one copy, dynamically shared, with the framework's deterministic planning +
  attestation.

This replaces bespoke host wiring with a real, conformance-tested substrate and
**unifies ducklink with sqlink** on one composition layer. Cost: it's an
integration of ducklink's loading/injection path (currently bespoke in
`ducklink-host`) onto `compose:dynlink` — mirror sqlink's `compose_provider.rs`.
The **wac de-risk does not need it**; introduce `compose:dynlink` at Phase 1.

## The ducklink function surface (reuse the build/query shape)

ML maps cleanly onto the **aggregate-fit + function-predict** pattern we already
built for the spatial index, with `env-id` as the model handle:

- **Fit = aggregate.** `ml_kmeans_fit(features..., k)` / `ml_linreg_fit(x..., y)`
  accumulates rows, calls `offload.run(env, {entry:"ducklink_ml.kmeans:fit", …})`
  at finalize, stores the fitted model in `env`, and returns the `env-id` (a
  handle — like the spatial-index build aggregate returns a handle).
- **Predict = scalar or table function.** `ml_kmeans_predict(env_id, features…)`
  / `ml_linreg_predict(env_id, x…)` calls
  `offload.run(env_id, {entry:"…:predict", args:<row or batch>})` and returns the
  prediction. Table-function form takes/returns batches.
- **Stateless one-shots** (no fit) as plain scalars where it makes sense:
  `ml_standardize(x)`, `ml_cosine_similarity(a, b)`.

Same handle-via-`SET VARIABLE` ergonomics as the spatial index (DuckDB rejects a
subquery as a table-fn arg), and the same generated-from-metadata approach so the
surface is declarative.

## Data path / codecs

- DuckDB is columnar; the ideal path is the **`arrow` codec** (batch in, batch
  out). **Risk:** the arrow codec likely needs `pyarrow`, which is **not** in
  pylon's wasm catalog (heavy C++). Confirm before relying on it.
- **v1 uses `msgpack`** (present in pylon: `packages-wasm/msgpack`): marshal
  columns as numpy-friendly arrays via msgpack; the ducklink side batches a vector
  → msgpack → numpy in Python → result → msgpack back. Row-at-a-time is the
  fallback but defeats numpy's vectorization — prefer batch (a chunk per call).
- Revisit `arrow`/zero-copy as an optimization once a lightweight arrow decoder
  (no pyarrow) is confirmed in pylon.

## ML scope — "something like scikit-learn", honestly

scikit-learn/scipy/pandas have **no wasm builds** in pylon; **numpy does**. So the
surface is **numpy-backed ML**, not `import sklearn`:
- Already proven: **k-means** (`pylon-kmeans-numpy`).
- Easy additions in pure numpy: standardize/normalize, linear & ridge regression
  (closed-form), logistic regression (GD), PCA (SVD), cosine/euclidean distance,
  train/test split, simple metrics (r², accuracy, RMSE).
- Out of scope (need scipy/sklearn): SVMs, gradient boosting, the full estimator
  API. Document the boundary so "ML" doesn't over-promise.

## env-id / model lifecycle

- A fitted model lives in an `env` keyed by `env-id` inside the resident pylon
  worker; predict calls reference it. Session-scoped (lives for the pylon
  instance) — same lifecycle decision as the spatial-index handle.
- Decide eviction (LRU / explicit `ml_drop(env_id)`) and whether `env-id` is
  content-addressed (pylon's full design) or a host-issued counter (simpler).

## Contract / versioning tie-in

- These are new `duckdb:extension` components, so they must be built against the
  **`@2.0.0`** contract (see `PLAN`-equivalent versioning work) and will exercise
  the new **load-time contract guard** — a good real-world test of it.
- The `py-offload` import is a **second, independent contract** (`tegmentum:
  py-offload@0.1.0`). Apply the same discipline: pin its version, and have the
  host's pylon worker and the function components agree on it. A pylon upgrade is
  its own (minor/major) bump.

## Dependencies & sequencing

1. Blocked on the ducklink **@2.0.0 versioning + v0.2.0** landing (the function
   components need the new contract). Do **not** start the ducklink-side before
   that, or they'll be built against the old contract.
2. **Independent now:** the **wac shared-pylon de-risk** — two trivial components
   importing `py-offload`, `wac plug` one pylon `py-worker`, confirm both run
   Python through the single shared copy. This validates the headline claim with
   zero dependence on the contract work.

## Phased plan

- **Phase 0 — de-risk (now, independent).** Prove one `py-worker` shared by ≥2
  importing components via `wac plug`; measure the size win (1×38 MB vs N×38 MB).
  Confirm `msgpack` round-trips numpy arrays end to end.
- **Phase 1 — host-mediated `py-offload` via `compose:dynlink`.** Wire ducklink's
  host onto `compose:dynlink@0.1.0` (mirror sqlink's `compose_provider.rs`): pylon
  becomes a `dynlink-provider` for `tegmentum:py-offload/offload`, each ML function
  a `dynlink-guest`, the host serving one resident pylon to all. One pylon,
  injected into every loaded ML component, on the shared orchestration substrate.
- **Phase 2 — the function pack.** `ducklink_ml` Python module (fit/predict for
  kmeans, linreg, logreg, pca, standardize, metrics) + the thin ducklink
  components (fit-aggregate + predict-scalar/table-fn) generated from a small
  manifest. Register under an `ml_` prefix.
- **Phase 3 — Arrow + browser.** If a no-pyarrow arrow path lands, switch the data
  codec to arrow for columnar zero-copy; verify the whole thing also runs in the
  jco/browser path (pylon already supports jco), tying into the web-API plan.

## Verification

- **Composition:** a build embedding K Python-ML functions contains pylon **once**
  (host-mediated: one instance serves all); show the artifact/footprint vs the
  naive K×38 MB.
- **Correctness:** `ml_kmeans_fit`/`predict` reproduces the standalone
  `pylon-kmeans-numpy` result; `ml_linreg` matches a numpy closed-form reference;
  a Python exception surfaces as a clean DuckDB error (via `raised(py-error)`),
  not a trap.
- **Contract guard:** the new components load only under the matching `@2.0.0`
  host and the matching `py-offload` version; a mismatch is rejected cleanly.
- **Smoke:** added to `tooling/smoke.py`; catalog verify clean.

## Open questions

- `arrow` codec without `pyarrow` — feasible in pylon, or msgpack-only for v1?
- One resident pylon worker + many concurrent calls: GIL serialization within one
  worker vs the `pool.py` multi-worker tier — is a single shared worker enough for
  DuckDB's execution model, or do we need a small pool (still "few" copies, not
  per-function)?
- CPython (38 MB, full numpy) vs MicroPython (smaller, limited numpy) per function
  class — pick per workload.
