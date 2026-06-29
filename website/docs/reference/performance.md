---
id: performance
title: Performance
sidebar_label: Performance
---

# Performance

This page records the measured cost of running DuckDB and its extensions as
WebAssembly, and what the [`@4.0.0` columnar ABI](../architecture/columnar-abi.md)
changed. The numbers are reported honestly: the wasm path is **substantially
faster than it was**, and the native-vs-wasm gap **narrowed**, but wasm is **not
yet near-native** in the dynamically-loaded configuration measured here.

## The harness

Benchmarks run from `benches/native-vs-wasm` (`run_matrix.py` + `matrix.json`),
contract-agnostic and artifact-only so the same matrix re-runs against any build.
The headline figures use the **slope method**: a regression over query count, so
the one-time wasm compile is excluded from the per-query number (see [AOT
compilation](../guides/deployment.md#aot-compilation) for the startup cost it
omits). Measurements below are Apple M2 Max, wasmtime 46.

## `@4.0.0` columnar + SIMD: absolute speedup

After the columnar ABI plus per-column SIMD kernels (the checksum/siphash
components rewritten column-at-a-time), the **wasm path itself** got faster versus
the `@3.1.0` row-major baseline:

| Workload | `@3.1.0` | `@4.0.0` | Speedup |
|---|---:|---:|---:|
| `talib` (aggregate) | 207 ms | 95 ms | −54% |
| `fnv1a` (light scalar) | 365 ms | 204 ms | −44% |
| `siphash` (heavy compute) | 562 ms | 364 ms | −35% |
| `aba` (dispatch-bound) | 364 ms | 299 ms | −18% |
| `aba-10M` (large) | 3602 ms | 3082 ms | −14% |

Compute-heavy and aggregate workloads benefited most — those are where the
per-column SIMD kernels and the elimination of per-cell variant marshalling pay
off.

## Native-vs-wasm overhead

The dynamically-loaded wasm path went from **~+110% over native (≈2.1× ) at
`@3.1.0` to ~+76% geomean at `@4.0.0`**. Best cases: `aba` +45%, `talib` +61%,
`aba-10M` +56%. (The lightest function, `fnv1a`, shows a larger ratio because it
is native-bound and the native baseline improved too — a ratio artifact, not a
regression.)

:::note Where the overhead lives
With compile excluded, the remaining overhead is two parts: the **wasm DuckDB
engine scan** (the engine itself running in wasm vs native — the irreducible
floor) **plus** the dispatch marshalling at the WIT boundary. The columnar ABI
erased the **dispatch** slice (boundary → memcpy); it does **not** touch the
engine-scan floor. So the gap narrowed but did not close.
:::

## Honest framing — what is *not* measured

The near-native upper bound needs two further configurations that are **not yet
measured** in this matrix:

- **AOT precompile** removes startup cost only (cold ≈8 s → `.cwasm` deserialize
  ≈0.08 s); it does not change the per-query slope.
- **Embedded extensions** remove the dispatch boundary entirely (the algorithm
  compiles into the core as a native scalar — see [embedded
  extensions](../guides/deployment.md#embedded-extensions)).

The embedded + AOT + columnar cell is the real "within X% of native" number, and
it is still blocked/unmeasured. **We do not claim a single-digit-percent
overhead** — that figure requires the embedded configuration and has not been
measured. The accurate summary today is: a hell of a lot better than it was, not
yet bragging territory.
