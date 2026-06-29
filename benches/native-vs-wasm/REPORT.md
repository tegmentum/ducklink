# Native-vs-WASM baseline @ 3.1.0-row-major

Host: Apple M2 Max | wasmtime 46.0.1 (823d1b8f2 2026-06-24) | darwin

Overhead = wasm marginal per-query cost / native marginal per-query cost - 1. Marginal cost is the regression slope over query count, so process spawn, wasm compile, extension load and table setup are excluded (they are the intercept, reported as `fixed`). Lower is better; a workload with `r2 < 0.99` is flagged as not defensible.

| workload | category | native ms/q | wasm ms/q | overhead | native rows/s | wasm rows/s | min r2 |
|---|---|--:|--:|--:|--:|--:|--:|
| aba_validate_1m | a. dispatch-bound scalar | 172.808 | 365.480 | +111.5% | 5.8M | 2.7M | 1.0000 |
| fnv1a_64_1m | b. light-compute scalar | 168.363 | 423.707 | +151.7% | 5.9M | 2.4M | 0.9966 |

**Aggregate (geomean of ratios, n=2): wasm is 2.31x native (+130.7% overhead).**

Per-workload detail (fixed = one-time cost excluded from the headline):

- **aba_validate_1m** (1,000,000 rows): native slope 172.808 ms/q (fixed 251 ms, r2 1.0000); wasm slope 365.480 ms/q (fixed 816 ms, r2 1.0000).
  - native cross-check (internal timer): 172.752 ms/q vs slope 172.808 ms/q.
- **fnv1a_64_1m** (1,000,000 rows): native slope 168.363 ms/q (fixed 402 ms, r2 1.0000); wasm slope 423.707 ms/q (fixed 171 ms, r2 0.9966).
  - native cross-check (internal timer): 168.798 ms/q vs slope 168.363 ms/q.
