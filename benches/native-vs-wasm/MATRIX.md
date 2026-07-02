# Native-vs-WASM configuration matrix @ 4.0.0-columnar

Host: Apple M2 Max | wasmtime 46.0.1 (823d1b8f2 2026-06-24) | darwin

Cells are the marginal per-query cost (regression slope over query count; process spawn, wasm compile, extension load and table setup are the intercept and are excluded). `fixed` is that intercept (the one-time/startup cost), reported separately. A cell with r2 < 0.99 is flagged.

## Configs

- **native** — NATIVE (bundled DuckDB + native ducklink ext). Baseline. Native engine builds DataChunks; reg_duckdb marshals across WIT into the component (embedded wasmtime). The extension algorithm is still wasm.
- **wasm-dynamic-noaot** — WASM DYNAMIC, no-AOT. Core + extension as separate components, host-resolver/WIT-dispatched, Cranelift-compiled at instantiation. The current shipped wasm path.
- **wasm-dynamic-aot** — WASM DYNAMIC, AOT. Same dynamic dispatch, but core + cli loaded from wasmtime-precompiled .cwasm (deserialize, no compile). Isolates the one-time compile cost from steady-state throughput.
- **wasm-embedded-noaot** — WASM EMBEDDED, no-AOT. Extension compiled INTO the core (embed framework: registered as a native scalar, no cross-component WIT dispatch boundary), compiled at instantiation.
- **wasm-embedded-aot** — WASM EMBEDDED, AOT. Embedded + precompiled .cwasm. The expected near-native upper bound once the columnar @4.0.0 ABI lands.

## Throughput — marginal per-query cost (ms/q, lower better)

| workload | rows | native | wasm-dynamic-noaot | wasm-dynamic-aot | wasm-embedded-noaot | wasm-embedded-aot |
|---|--:|--:|--:|--:|--:|--:|
| aba_validate_1m | 1,000,000 | 203.60 | 305.15 | _skipped_ | 98.78 | _skipped_ |
| fnv1a_64_1m | 1,000,000 | 86.18 | 205.34 | _skipped_ | 99.77 | _skipped_ |
| siphash_long_1m | 1,000,000 | 174.67 | 373.89 | _skipped_ | 168.80 | _skipped_ |
| talib_sma_1m | 1,000,000 | 57.94 | 91.93 | _skipped_ | _skipped_ | _skipped_ |
| aba_validate_10m | 10,000,000 | 1947.86 | 2960.91 | _skipped_ | 983.04 | _skipped_ |

## Overhead vs native (%, lower better)

| workload | native | wasm-dynamic-noaot | wasm-dynamic-aot | wasm-embedded-noaot | wasm-embedded-aot |
|---|--:|--:|--:|--:|--:|
| aba_validate_1m | +0% | +50% | - | -51% | - |
| fnv1a_64_1m | +0% | +138% | - | +16% | - |
| siphash_long_1m | +0% | +114% | - | -3% | - |
| talib_sma_1m | +0% | +59% | - | - | - |
| aba_validate_10m | +0% | +52% | - | -50% | - |

## Fixed cost — one-time startup (ms; spawn + compile/deserialize + load + setup)

| workload | native | wasm-dynamic-noaot | wasm-dynamic-aot | wasm-embedded-noaot | wasm-embedded-aot |
|---|--:|--:|--:|--:|--:|
| aba_validate_1m | 193 | 557 | - | 602 | - |
| fnv1a_64_1m | 249 | 689 | - | 680 | - |
| siphash_long_1m | 406 | 804 | - | 805 | - |
| talib_sma_1m | 112 | 463 | - | - | - |
| aba_validate_10m | 2557 | 2288 | - | 2252 | - |

## Key deltas

### 1. Each config vs native (geomean overhead across workloads)

| config | geomean ratio vs native | overhead | n |
|---|--:|--:|--:|
| native | 1.00x | +0.0% | 5 |
| wasm-dynamic-noaot | 1.79x | +79.1% | 5 |
| wasm-embedded-noaot | 0.72x | -27.7% | 4 |

### 2. Dynamic vs embedded — the dynamic-loading dispatch-boundary cost

- **noaot**: embedded is 2.55x faster than dynamic (geomean); i.e. the host-mediated dispatch boundary is ~61% of the dynamic per-query cost.
  - aba_validate_10m: dynamic 2960.91 ms/q vs embedded 983.04 ms/q (67% boundary).
  - aba_validate_1m: dynamic 305.15 ms/q vs embedded 98.78 ms/q (68% boundary).
  - fnv1a_64_1m: dynamic 205.34 ms/q vs embedded 99.77 ms/q (51% boundary).
  - siphash_long_1m: dynamic 373.89 ms/q vs embedded 168.80 ms/q (55% boundary).
- **aot**: not measurable (embedded config unavailable in this run).

### 3. AOT vs no-AOT — compile/startup cost (slope must be unchanged)

- **dynamic**: not measurable (one AOT tier unavailable).
- **embedded**: not measurable (one AOT tier unavailable).

## Skipped configs (this run)

- **wasm-dynamic-aot**: artifacts absent (no core)
- **wasm-embedded-aot**: artifacts absent (no core)
- **wasm-embedded-noaot**: artifacts absent (no core)

See `README.md` ("Status of the embedded configs" / AOT) for why these are not built here and the expected magnitudes; the harness measures them with no code change once `NVW_AOT` / `NVW_EMBED` artifacts exist.

## Interpretation

The shipped wasm path (dynamic, no-AOT) is **1.79x native** (geomean) on steady-state throughput, with the gap widest on the dispatch-/compute-bound scalars and narrowest on the aggregate. Because the slope method excludes one-time compile (it lands in `fixed`), this gap is the **per-row engine + dispatch** cost, not compile: it is the wasm DuckDB core's per-row scan plus the host-mediated cross-component WIT marshalling. AOT (config 3) targets only the `fixed` column (the one-time core compile) and by construction leaves the slope unchanged; embedding (configs 4-5) is the lever that attacks this slope by removing the dispatch boundary. The @4.0.0 columnar ABI is expected to cut the same slope further.

