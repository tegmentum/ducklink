# Methodology — making the native-vs-wasm number defensible

This document exists so the headline ("the wasm distribution path is within X%
of native") survives scrutiny. It states exactly what is measured, what is
deliberately excluded, the known asymmetries, and how the @4.0.0 re-run is
structured.

## 1. What "native" and "wasm" mean here

Both paths execute the **identical** wasm `duckdb:extension` component. They
differ only in the host around it:

- **native** — a native (bundled) DuckDB engine + the native `ducklink`
  extension. The engine builds DataChunks natively; `reg_duckdb` marshals them
  across the WIT boundary into the component (run by an embedded wasmtime).
- **wasm** — the DuckDB core compiled to wasm, run under wasmtime/wasi; it loads
  the same component and marshals to it across the in-wasm component boundary.

Consequence, stated honestly: the extension *algorithm* is wasm in **both**
paths. So this benchmark does **not** measure "wasm algorithm vs native
algorithm". It measures the cost of the **engine + dispatch/marshalling layer
being wasm**. That is the right thing for the distribution claim (a user choosing
DuckDB-wasm gets the same component; the question is what the wasm engine around
it costs), and it predicts the shape of the results:

- **dispatch-bound** (trivial guest, e.g. `aba_validate`): the wasm-engine
  per-row scan + the wasm marshalling are the whole cost ⇒ largest gap.
- **scan-bound / large-data**: the wasm engine's table scan dominates ⇒ gap
  tracks raw wasm-vs-native engine throughput.
- **compute-heavy guest** (e.g. `siphash` over long inputs, `talib` aggregate):
  the guest work — wasm in both paths — dominates and dilutes the host
  difference ⇒ converges toward parity. We report this truthfully rather than
  cherry-picking it as the headline.

## 2. The measurement: slope over query count

The dominant cost in this stack is **one-time wasm Cranelift compilation** of
the ~48 MB core (memory: `component-compile-cache`), plus process spawn,
extension load, and table setup. A naive "time one query" would be swamped by
these. We isolate the marginal per-query cost with a slope method that is
identical for both paths:

For each (path, workload) we run the query `K` times back-to-back inside one
process and time the whole process externally, for several `K` (the first is
always `K = 0`). We fit by least squares:

```
wall_time(K) = intercept + slope * K
```

- `slope` = **marginal per-query cost** — the reported number.
- `intercept` = the one-time cost (spawn + wasm compile + load + setup). This is
  exactly the "compile" we are told to exclude; it cancels in the slope and is
  ALSO reported separately as `fixed_cost_ms`.

This is why the method is defensible: every fixed cost — including the wasm
compile that would otherwise dominate — lands in the intercept and is removed
from the headline by construction, symmetrically for both paths.

### Defensibility gates

- **r² gate**: a workload whose fit r² < 0.99 is flagged (`⚠`) in the report and
  must not be quoted — a poor fit means the slope is not a clean per-query cost.
- **Warm**: each process runs a warmup query before the timed loop; the wasmtime
  compile cache is on (host `build_engine`), so the intercept is warm-compile.
- **Re-parse parity**: both paths re-parse/plan/execute each iteration (native
  re-`prepare`s; the wasm CLI re-reads each statement) so the slope includes the
  same parse+plan+execute envelope on both sides.
- **Single-row results**: every query aggregates to one scalar, so result
  formatting/IO is negligible and symmetric.
- **Independent cross-check**: the native runner also reports its internally
  timed K-loop (`internal_ms`); `internal_ms/K` should match the external slope.
  Agreement validates that the external slope is not an artifact of process
  accounting.
- **Repetitions + variance**: each (path, K) is repeated (`reps`); the headline
  fits the per-K medians, and per-rep slopes give the slope distribution
  (`slope_ms_median/p25/p75`).

## 3. Known asymmetries (disclosed, not hidden)

- **Two timing mechanisms looked at**: the wasm CLI does not implement `.timer`,
  so we time externally. To keep native comparable we time it externally too
  (same slope method) rather than using its in-process timer — and we publish
  the in-process number only as a cross-check.
- **Nested vs single wasmtime**: in the wasm path the core and the component run
  under one wasmtime; in the native path only the component does. This is
  inherent to the two distribution paths, not a harness artifact.
- **CPU pinning**: macOS offers no taskset; we counter run-to-run scheduler
  noise with medians, repetitions, and the r² gate rather than affinity. On
  Linux, pin with `taskset -c` and disable turbo for the tightest numbers.

## 4. Aggregate

The honest aggregate is the **geometric mean** of the per-workload
`wasm/native` ratios. Geomean is the correct average for ratios and prevents any
single workload (e.g. the dispatch-bound worst case) from dominating either
direction. We report it as "wasm is N.NNx native" with the per-workload table
alongside, never a single cherry-picked workload.

## 5. @4.0.0 re-run (columnar dispatch)

The @3.1.0 baseline is **row-major** dispatch (`list<list<duckvalue>>`, a tagged
variant per cell). #185 introduces the **columnar** ABI
(`list<colvec>`, one typed contiguous buffer per argument). The standalone
boundary prototype (`benches/columnar-abi-prototype`) already shows ~82-110x at
the raw WIT crossing; this harness measures what that does to the **end-to-end**
native-vs-wasm overhead.

Re-run is artifact-only:

1. Build the @4.0.0 core + components in a separate checkout (don't contend with
   the live build): point `DUCKLINK_REPO` at it.
2. Rebuild `native-runner` against the @4.0.0 submodule (`reg_duckdb` must speak
   the columnar ABI the components now expect).
3. `CONTRACT_LABEL=4.0.0-columnar ./run.py` → `results/baseline-4.0.0-columnar.json`.

The columnar headline is the **reduction in `overhead_pct`** per workload
between the two result files. Expected shape (to be confirmed, not promised):
the dispatch-bound and large-data rows narrow the most (marshalling is their
whole tax); compute-heavy rows were already near parity and move little. The
per-workload structure is identical across versions, so the two JSON files diff
cleanly.

## 6. The configuration matrix (`run_matrix.py`)

`run.py` answers "what does the wasm path cost vs native"; `run_matrix.py`
answers "*where* does that cost live" by running the identical slope method
across a 5-config matrix (see `README.md`). Two design points keep the
attribution honest:

### Slope vs fixed cost — which axis surfaces where

Every cell records both the regression **slope** (marginal per-query cost =
throughput) and the **intercept** (`fixed_cost_ms` = one-time startup). The two
matrix axes are deliberately read from different numbers:

- **embedded vs dynamic** is a **slope** question. Embedding compiles the
  extension into the core as a native scalar, removing the per-row, host-mediated
  cross-component WIT dispatch — that is per-query work, so it moves the slope.
  We report `dynamic_slope / embedded_slope` (and the boundary as a % of the
  dynamic slope).
- **AOT vs no-AOT** is a **fixed-cost** question. AOT replaces the Cranelift
  compile of the ~50 MB core with a `.cwasm` deserialize; it touches the one-time
  startup, not the steady-state per-row work. So the headline for AOT is the
  drop in `fixed_cost_ms`, and we *verify* the slope ratio AOT/no-AOT ≈ 1.0 — if
  AOT changed the slope, something other than compile was being measured. (Note:
  the no-AOT fixed cost is already a *warm* Cranelift compile, because the host
  wasmtime disk cache is on; AOT is measured against that warmed baseline, not a
  cold compile, so the reported saving is conservative.)

### Honest scope of the AOT config

Only the core + cli are precompiled to `.cwasm`. The ~50 MB core is ~99% of the
one-time compile; the ~100 KB extension components are negligible and the
resolver loads them by `<name>.wasm`, so they stay JIT. This is disclosed rather
than hidden: the AOT config isolates the *core* compile, which is the cost that
actually dominates startup.

### Honest scope of the embedded configs

The embedded configs require the embed framework (extension algorithm compiled
into the core as a native scalar). When that framework is not built for the
target contract, the configs are reported `skipped` with the reason rather than
faked — a partial matrix is still publishable, and the known magnitude of the
embed win is cited from prior measurement (see `README.md`). The harness
consumes embedded artifacts from `NVW_EMBED` with no code change once they exist.

### Coherent-artifact requirement

All wasm artifacts in a run (core, cli, every extension) and the `HOST_BIN` must
share the same contract MAJOR. A host built at a different MAJOR rejects the
components ("targets contract N.x but host speaks M.x"). The matrix is therefore
run against a *staged*, version-pinned artifact set, never a live build tree
that another branch may be re-stamping underneath it.
