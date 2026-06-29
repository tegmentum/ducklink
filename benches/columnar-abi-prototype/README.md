# Columnar ABI prototype — row-major vs typed-column dispatch over a real WIT boundary

This is the GO/NO-GO benchmark for the proposed major-4 **columnar dispatch
ABI**. It crosses a real wasmtime component boundary (canonical ABI, the same
one `duckdb:extension` rides) two ways, doing the identical `+1` i64 scalar over
1M rows in 2048-row chunks:

- **row-major** — `call-scalar-batch(rows: list<list<duckvalue>>) -> list<duckvalue>`,
  the current `@3.1.0` shape. `duckvalue` is the faithful 22-arm tagged variant.
- **columnar** — `call-scalar-batch-col(args: list<colvec>) -> colvec`, the proposed
  shape: one typed contiguous buffer per argument (`list<s64>`) + a packed
  validity bitmap.

The guest is a real component built with `cargo component`; the host is real
`wasmtime` 46 (the same version ducklink-host/runtime pin). Checksums of both
paths are asserted equal, so the comparison is correctness-preserving.

## Result (Apple silicon, wasmtime 46.0.1, release)

```
ROW-MAJOR  list<list<duckvalue>>  :  1891.94 ms total     94.65 ns/row
COLUMNAR   list<colvec>           :    17.09 ms total      0.85 ns/row
speedup: 110.72x   latency reduction: 99.1%

--- boundary-only (inputs prebuilt; isolates the canonical-ABI crossing) ---
ROW-MAJOR  :   73.48 ns/row
COLUMNAR   :    0.89 ns/row
speedup: 82.58x   latency reduction: 98.8%
```

The row-major boundary spends ~73 ns/row serializing/deserializing a tagged
variant per cell plus a `list<list<>>` reallocation per row; the columnar
boundary is a bulk `memcpy` of the typed buffer (~0.9 ns/row). This is the
single largest remaining lever in the dispatch path — every row-major-compatible
optimization (batched dispatch, compile cache, precompile, guest scratch reuse,
per-column FFI hoist) is already landed.

## Reproduce

```sh
cd guest && cargo component build --release && cd ..
cd host  && cargo run --release
```
