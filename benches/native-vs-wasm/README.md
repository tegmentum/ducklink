# native-vs-wasm — head-to-head ducklink extension benchmark

Measures the overhead of the **wasm distribution path** relative to **native**
for ducklink extensions, across a representative workload spectrum, so the
headline number ("the wasm path is within X% of native") is defensible.

## What is compared

The SAME wasm `duckdb:extension` component + SAME SQL + SAME data, run two ways:

| path | engine | dispatch glue | extension logic |
|------|--------|---------------|-----------------|
| **native** | native DuckDB (bundled) | native `ducklink` extension (`reg_duckdb`) | the wasm component (wasmtime) |
| **wasm**   | DuckDB core compiled to wasm (wasmtime/wasi) | wasm core | the same wasm component |

Note the extension *algorithm* runs as wasm in **both** paths (it is the same
component). The measured native-vs-wasm delta is therefore the cost of the
**engine + dispatch layer being wasm**, not of the algorithm — see
`METHODOLOGY.md` for why this is the right, honest framing and why
compute-heavy workloads converge toward parity while dispatch/scan-bound
workloads show the gap.

## Workloads (`workloads.json`)

| id | category | function |
|----|----------|----------|
| `aba_validate_1m`  | (a) dispatch-bound scalar | `aba_validate` (isolates dispatch tax) |
| `fnv1a_64_1m`      | (b) light-compute scalar  | `fnv1a_64` |
| `siphash_long_1m`  | (c) heavier-compute scalar | `siphash` over ~256-byte inputs (guest-dominated) |
| `talib_sma_1m`     | (c) compute-heavy aggregate | `sma` global aggregate |
| `aba_validate_10m` | (d) large data | `aba_validate` over 10M rows (engine-scan scaling) |

## Prerequisites (uses prebuilt @3.1.0 artifacts — no heavy wasm rebuild)

1. The ducklink host binary and the prebuilt wasm core + components, in a
   checkout pointed at by `DUCKLINK_REPO` (default `~/git/ducklink`):
   - `$DUCKLINK_REPO/target/release/ducklink` (`make host`)
   - `$DUCKLINK_REPO/target/wasm32-wasip2/release/ducklink_core.wasm` and `ducklink_cli.wasm`
   - `$DUCKLINK_REPO/artifacts/extensions/{aba,checksums,siphash,talib}.wasm`
2. The native runner (one-time bundled-DuckDB compile). It is built *inside* the
   native extension submodule so it uses that crate's pinned `Cargo.lock`
   (a separate path-dep crate re-resolves a transitive dep and fails to compile):
   ```sh
   git submodule update --init native-extension/ducklink   # if not already
   ./native-runner/build.sh
   ```
   Produces `native-extension/ducklink/target/release/nvw_native_runner`.

## Run

```sh
./run.py                # full run -> results/baseline-<label>.json + REPORT.md
./run.py --quick        # 2 reps (smoke)
./run.py --only aba_validate_1m
./run.py --verify-only  # check every workload resolves on both paths
```

Config via env: `DUCKLINK_REPO`, `HOST_BIN`, `EXT_DIR`, `NATIVE_BIN`,
`CONTRACT_LABEL` (see the top of `run.py`).

## Re-running against @4.0.0 (columnar dispatch, after #185 lands)

The harness is contract-agnostic — only the artifacts change. After #185
(`feat/wit-4.0.0`) rebuilds the core + components at the columnar ABI:

```sh
DUCKLINK_REPO=/path/to/the/4.0.0/checkout \
CONTRACT_LABEL=4.0.0-columnar \
./run.py
```

Rebuild `native-runner` against the @4.0.0 submodule first (the native
extension's `reg_duckdb` must speak the same ABI as the components). The
columnar headline is the **drop in `overhead_pct`** on the dispatch- and
scan-bound rows between `results/baseline-3.1.0-row-major.json` and
`results/baseline-4.0.0-columnar.json`. See `METHODOLOGY.md` §"@4.0.0 re-run".

---

# Configuration matrix (`run_matrix.py`)

`run.py` compares 2 paths (native vs the shipped wasm path). `run_matrix.py`
extends that to the full **5-config matrix** so the wasm overhead can be
attributed to *where* it lives (dispatch boundary vs compile vs execution):

| # | config id | engine | dispatch | compile |
|---|-----------|--------|----------|---------|
| 1 | `native` | native DuckDB + native ducklink ext | WIT (native→component) | JIT (intercept) |
| 2 | `wasm-dynamic-noaot` | wasm core | host-resolver / WIT, separate components | JIT at instantiation |
| 3 | `wasm-dynamic-aot` | wasm core | host-resolver / WIT | **AOT** (`.cwasm` deserialize) |
| 4 | `wasm-embedded-noaot` | wasm core | **none** (extension compiled into core as native scalar) | JIT |
| 5 | `wasm-embedded-aot` | wasm core | none | AOT |

Same `workloads.json`, same SQL/data, same slope method as `run.py`, so every
cell is directly comparable. We report **both** the slope (marginal per-query
cost = throughput) and the intercept (`fixed_cost_ms` = one-time startup),
because the two axes surface in different places:

- **embedded vs dynamic** → the **slope** (the per-row dispatch boundary).
- **AOT vs no-AOT** → the **fixed cost** (the one-time compile); the slope must
  be unchanged (AOT changes startup, not steady-state throughput).

## Configs the matrix produces (`matrix.json`)

Each config resolves its artifacts from roots passed via env:

```sh
HOST_BIN=/abs/ducklink            # a host whose CONTRACT_MAJOR matches the artifacts
NATIVE_BIN=/abs/nvw_native_runner # the prebuilt native runner
NVW_ART=./artifacts-3.1.0         # core.wasm, cli.wasm, extensions/<n>.wasm
NVW_AOT=$NVW_ART/aot              # core.cwasm, cli.cwasm  (from `ducklink precompile`)
NVW_EMBED=$NVW_ART/embedded       # <ext>/ducklink_core.wasm (+ .cwasm) per embedded ext
./run_matrix.py                   # -> results/matrix-<label>.json + MATRIX.md
```

A config whose artifacts are absent (or whose cell doesn't resolve) is reported
as **skipped** with a reason rather than aborting — a partial matrix is still
publishable.

## Building each config's artifacts

- **dynamic, no-AOT** — the prebuilt `core.wasm` + `cli.wasm` + the `*.wasm`
  components. (Stage a coherent set: a core whose `CONTRACT_MAJOR` matches the
  components and the `HOST_BIN`. A host built at a different contract MAJOR
  rejects the components with "targets contract N.x but host speaks M.x".)
- **dynamic, AOT** — `ducklink precompile core.wasm core.cwasm` (and cli). The
  50 MB core dominates the one-time compile (~minutes cold); the ~100 KB
  components are negligible, so only core+cli are precompiled.
- **embedded** — the **embed framework** (`make core-embed EMBED=embed-<name>` /
  `ducklink compose --embed <name>`): compile the extension's algorithm INTO the
  core as a native scalar (no cross-component WIT dispatch). See the
  *Status* note below.

## Status of the embedded configs (4, 5)

At @3.1.0 in this checkout the embedded configs are **not buildable** and are
reported `skipped`:

- `make core-embed` is disabled — *"the embed framework moved ducklink-side in
  the duckdb-wasm split"* — and `ducklink compose` needs a `crates/ducklink-core`
  crate that does not exist here; there are no `embed-<name>` features.
- wac-composing the *dynamic* dispatch interface into the core does **not**
  yield a working embedded extension either: the standalone (composed) loader is
  a no-op stub that **declines all extension loads** (registration is
  host-driven), so a composed core has no registered function.
- Producing them needs the ducklink-side embed overlay **plus** a heavy wasm
  core rebuild (`DUCKDB_STATIC_LIB`), which also contends with the live @4.0.0
  build — out of scope for this bench pass.

The expected magnitude is known from the dispatch-perf work (memory
`extension-dispatch-perf`): on a 1M-row `isin` scalar the embed framework took
the dynamic path from **1.88 s → 1.31 s (~30% faster)** — i.e. the
host-mediated dispatch boundary is ~30% of the dynamic per-query cost — and it
ran in the **standalone** (no host), which the dynamic path cannot. Drop the
embedded artifacts into `NVW_EMBED` and the matrix measures them with no code
change.

## Status of the AOT config (3) @3.1.0

The AOT config is *buildable in principle* (`ducklink precompile core.wasm
core.cwasm`) and the harness loads `.cwasm` via `--core-component`. On this run
it is reported `skipped` because the precompile could **not be produced on this
host while the @4.0.0 build (#187) was running**: wasmtime's Cranelift
precompile of the ~50 MB core needs a ~14 GB resident working set to *serialize*
the compiled module to `.cwasm`, and under the concurrent build's memory waves
the OS jetsam OOM-killed it (4 attempts; `RAYON_NUM_THREADS=1` cut the *compile*
peak but the *serialize* still needs the whole module resident and thrashed).
This is the "do not contend with #187" hazard, not a harness limitation — on an
unloaded host (or after #187 finishes) `make precompile` produces the `.cwasm`
and the AOT column fills in with no code change.

What AOT would show is determinable without the artifact, and is stated honestly
rather than faked:

- **Slope (throughput): unchanged.** AOT only swaps the core's Cranelift compile
  for a `.cwasm` deserialize; per-row execution is identical, so the AOT slope
  equals the dynamic-no-AOT slope by construction (the slope method already
  excludes compile). So config 3's throughput row == config 2's.
- **Fixed cost (startup): large drop.** The dynamic-no-AOT `fixed` measured here
  is a *warm* Cranelift compile (the wasmtime disk cache is on) — ~0.6-0.8 s of
  intercept on the 1M rows. A `.cwasm` deserialize is ~0.08 s (memory
  `component-compile-cache`), and on a *cold* host the gap is ~8 s -> ~0.08 s.
  So AOT's win is concentrated in startup, ~0.5-0.7 s/process warm (much more
  cold), with no steady-state throughput change.

## Results @3.1.0 (this run)

Apple M2 Max, wasmtime 46.0.1. Marginal per-query cost (regression slope; r2 ~=
1.0000 on every measured cell), one-time compile excluded:

| workload | native ms/q | dynamic-wasm ms/q | overhead |
|---|--:|--:|--:|
| aba_validate_1m (dispatch-bound scalar) | 174.0 | 364.1 | +109% |
| fnv1a_64_1m (light-compute scalar) | 168.4 | 365.4 | +117% |
| siphash_long_1m (heavier-compute scalar) | 230.9 | 561.8 | +143% |
| talib_sma_1m (compute-heavy aggregate) | 116.2 | 207.4 | +78% |
| aba_validate_10m (large data, 10M) | 1739.5 | 3601.9 | +107% |

**Geomean: dynamic wasm is 2.10x native (+110%)** — consistent with #186's
+130% (which sampled only the two dispatch-bound rows). The gap is the per-row
**engine + dispatch** cost (it survives the compile-exclusion): it is widest on
the compute-heavy `siphash` and narrowest on the `talib` aggregate (whose
whole-group marshalling amortizes the per-row boundary). AOT (3) and embedded
(4-5) are blocked here as above; embedded is the lever that attacks this slope
(prior measurement: ~30% on 1M-row isin), AOT only attacks startup.

## Re-running the matrix against @4.0.0 (columnar)

Artifact-only, same as `run.py`'s baseline re-run. Point the env at a @4.0.0
build:

```sh
HOST_BIN=/abs/4.0.0/ducklink \
NVW_ART=/abs/4.0.0/artifacts \
CONTRACT_LABEL=4.0.0-columnar \
./run_matrix.py
```

(Precompile the @4.0.0 core/cli into `$NVW_ART/aot` and, once the embed overlay
lands, drop the embedded cores into `$NVW_ART/embedded`.) The headline @4.0.0
matrix is where **embedded + AOT + columnar** should be the near-native upper
bound; diff `results/matrix-3.1.0-row-major.json` vs
`results/matrix-4.0.0-columnar.json` cell-by-cell.
