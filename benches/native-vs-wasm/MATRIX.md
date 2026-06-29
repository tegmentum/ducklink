# Native-vs-WASM configuration matrix @ 3.1.0-row-major

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
| aba_validate_1m | 1,000,000 | 174.02 | 364.05 | _skipped_ | _skipped_ | _skipped_ |
| fnv1a_64_1m | 1,000,000 | 168.35 | 365.39 | _skipped_ | _skipped_ | _skipped_ |
| siphash_long_1m | 1,000,000 | 230.90 | 561.77 | _skipped_ | _skipped_ | _skipped_ |
| talib_sma_1m | 1,000,000 | 116.24 | 207.44 | _skipped_ | _skipped_ | _skipped_ |
| aba_validate_10m | 10,000,000 | 1739.50 | 3601.92 | _skipped_ | _skipped_ | _skipped_ |

## Overhead vs native (%, lower better)

| workload | native | wasm-dynamic-noaot | wasm-dynamic-aot | wasm-embedded-noaot | wasm-embedded-aot |
|---|--:|--:|--:|--:|--:|
| aba_validate_1m | +0% | +109% | - | - | - |
| fnv1a_64_1m | +0% | +117% | - | - | - |
| siphash_long_1m | +0% | +143% | - | - | - |
| talib_sma_1m | +0% | +78% | - | - | - |
| aba_validate_10m | +0% | +107% | - | - | - |

## Fixed cost — one-time startup (ms; spawn + compile/deserialize + load + setup)

| workload | native | wasm-dynamic-noaot | wasm-dynamic-aot | wasm-embedded-noaot | wasm-embedded-aot |
|---|--:|--:|--:|--:|--:|
| aba_validate_1m | 235 | 610 | - | - | - |
| fnv1a_64_1m | 337 | 707 | - | - | - |
| siphash_long_1m | 462 | 833 | - | - | - |
| talib_sma_1m | 189 | 446 | - | - | - |
| aba_validate_10m | 2295 | 2255 | - | - | - |

## Key deltas

### 1. Each config vs native (geomean overhead across workloads)

| config | geomean ratio vs native | overhead | n |
|---|--:|--:|--:|
| native | 1.00x | +0.0% | 5 |
| wasm-dynamic-noaot | 2.10x | +110.0% | 5 |

### 2. Dynamic vs embedded — the dynamic-loading dispatch-boundary cost

- **noaot**: not measurable (embedded config unavailable in this run).
- **aot**: not measurable (embedded config unavailable in this run).

### 3. AOT vs no-AOT — compile/startup cost (slope must be unchanged)

- **dynamic**: not measurable (one AOT tier unavailable).
- **embedded**: not measurable (one AOT tier unavailable).

## Skipped configs (this run)

- **wasm-dynamic-aot**: artifacts absent (no core)
- **wasm-embedded-noaot**: artifacts absent (no core)
- **wasm-embedded-aot**: artifacts absent (no core)

See `README.md` ("Status of the embedded configs" / AOT) for why these are not built here and the expected magnitudes; the harness measures them with no code change once `NVW_AOT` / `NVW_EMBED` artifacts exist.

## Interpretation

The shipped wasm path (dynamic, no-AOT) is **2.10x native** (geomean) on steady-state throughput, with the gap widest on the dispatch-/compute-bound scalars and narrowest on the aggregate. Because the slope method excludes one-time compile (it lands in `fixed`), this gap is the **per-row engine + dispatch** cost, not compile: it is the wasm DuckDB core's per-row scan plus the host-mediated cross-component WIT marshalling. AOT (config 3) targets only the `fixed` column (the one-time core compile) and by construction leaves the slope unchanged; embedding (configs 4-5) is the lever that attacks this slope by removing the dispatch boundary. The @4.0.0 columnar ABI is expected to cut the same slope further.

