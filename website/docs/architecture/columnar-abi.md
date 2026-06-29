---
id: columnar-abi
title: The @4.0.0 columnar ABI
sidebar_label: Columnar ABI (@4.0.0)
---

# The `@4.0.0` columnar ABI

The `duckdb:extension` contract is now **major-4**. The single change that earned
the major bump is the **columnar hot dispatch path**: per-`DataChunk` scalar,
aggregate, and cast calls now cross the WIT canonical ABI as **typed columns**
(one bulk transfer per fixed-width column) instead of the row-major
`list<list<duckvalue>>` tagged-variant batch that major-3 used. That, in turn,
unlocks per-column SIMD kernels in the guest.

## The row-major problem

Through `@3.x` the batch entry point was row-major:

```wit
call-scalar-batch(rows: list<list<duckvalue>>) -> list<duckvalue>
```

`duckvalue` is a ~24-byte tagged variant, so the boundary serialized a tag +
payload **per cell** and reallocated an inner `list<>` **per row** — `O(rows ×
cols)` of branchy, non-vectorizable work. After every row-major-compatible lever
was already landed (batched dispatch, the compile cache and `.cwasm` precompile,
guest scratch reuse, per-column FFI pointer/validity hoist), this marshalling was
measured as the single largest remaining item in the dispatch path (~73 ns/row on
a 1M-row `i64` scalar).

DuckDB is a **vectorized** engine — its vectors are already flat contiguous
arrays. The columnar ABI stops fighting that.

## The columnar contract

The hot path now passes each argument as a **typed, contiguous column** plus a
packed validity bitmap. The value model is `column-types.wit`:

```wit
// A typed, contiguous COLUMN: one buffer per DuckDB physical type. The core
// fills each arm by memcpy-ing straight from duckdb_vector_get_data.
variant column {
  boolean(list<bool>),
  int64(list<s64>),
  uint64(list<u64>),
  float64(list<f64>),
  int32(list<s32>),
  timestamp(list<s64>),
  // ... the full fixed-width family (int8/16/32, uint8/16/32, float32,
  //     date, time, timestamptz, decimal, interval, uuid) ...
  text(list<string>),       // variable-width: element-wise, as before
  blob(list<list<u8>>),     // variable-width: element-wise, as before
  complex(list<complexvalue>),  // the escape hatch (type-expr + json)
}

// A column plus its validity + row count — the unit of columnar transfer.
record colvec {
  data: column,
  validity: list<u8>,   // packed LE bitmap; empty = all-valid fast path
  rows: u32,
}
```

The dispatch surface (`callback-dispatch.wit`) is correspondingly columnar on the
hot path, with the cold singleton paths kept row-major for ergonomics:

```wit
// HOT PATH — one columnar call per DataChunk.
call-scalar-batch-col: func(handle: u32, args: list<colvec>, ctx: invokeinfo)
  -> result<colvec, duckerror>;
call-aggregate-col:    func(handle: u32, args: list<colvec>) -> result<duckvalue, duckerror>;
call-cast-col:         func(handle: u32, arg: colvec) -> result<colvec, duckerror>;

// COLD singletons (non-batched / edge paths) stay row-major duckvalue.
call-scalar: func(handle: u32, args: list<duckvalue>, ctx: invokeinfo) -> result<duckvalue, duckerror>;
call-table:  func(handle: u32, args: list<duckvalue>) -> result<resultset, duckerror>;
call-pragma: func(handle: u32, args: list<duckvalue>) -> result<option<duckvalue>, duckerror>;
call-cast:   func(handle: u32, value: duckvalue) -> result<duckvalue, duckerror>;
```

Key properties:

- **Fixed-width columns cross as a single bulk memcpy.** Because DuckDB vectors
  are already flat, the core copies straight from `duckdb_vector_get_data` into a
  `colvec` arm — no per-cell read loop.
- **NULL is out-of-band.** `validity` is byte-for-byte DuckDB's own validity mask
  (bit *i* set ⇒ row *i* valid); an **empty** bitmap means "all valid" (zero
  allocation). The data buffer therefore stays a flat typed array.
- **Variable-width and nested data are unchanged.** `text` / `blob` stay
  element-wise (unavoidable for var-len data, and no worse than the row-major
  path). Nested/future types still ride the `complex(type-expr, json)` escape
  hatch — see [the type contract](type-contract.md).

This is what unlocks **per-column SIMD kernels** in the guest: a `list<s64>`
argument arrives as a contiguous slice the extension can run a vectorized kernel
over, rather than a per-cell variant it must branch on. The compute-heavy
checksum/siphash components were rewritten as column-at-a-time kernels on this
path; see [performance](../reference/performance.md).

## The contract-versioning & stability model

A WebAssembly extension that ships once and runs for years needs a stable
**external** surface even though DuckDB's internal C++ extension ABI churns every
release. ducklink resolves that with **one frozen external contract** and a
**single binding layer** that absorbs the internal churn.

### The contract identity is a content digest

The authoritative identity of the contract is **not** the semver — it is a
content-addressed digest over the canonical WIT bytes (`witcanon:1`,
`CONTRACT_DIGEST` in `crates/ducklink-runtime/build.rs`). The current `@4.0.0`
contract digest is:

```
a2ad9764ac971345d6a650b92edbda034b160980acf148d354126f7e6f92ba40
```

Every catalog entry records the digest it was built against (`wit_contract` +
`wit_contract_version` in `registry/index.json`), and conformance results are
stamped with the same `at:` digest. The digest changes **iff the contract shape
changes**; the `@MAJOR` semver is just its coarse runtime proxy in the loader
guard.

### Two tiers behind one surface

- **The common tier is pegged to DuckDB's stable C Extension API**
  (`duckdb_ext_api_v1`, frozen since DuckDB **1.2.0**): scalar, table, aggregate,
  cast, replacement-scan route through `duckdb_create_*` / `duckdb_register_*`
  symbols. A DuckDB version bump does **not** touch these.
- **The C++-only tier** (storage, index, optimizer, collation, secret, parser)
  has no stable C anchor, so its churn is **quarantined in the wasm core's C++
  shims** ([the C++ shim pattern](capability-surface.md#the-two-foundational-techniques)).
  When DuckDB's internal classes move, the fix lands once, in the core's binding
  layer — the WIT contract and every shipped component stay byte-identical.

### Why this was a major (and the freeze that follows)

Major bumps are deliberately rare. New **types** ride the `complex()` escape hatch
(no bump); new **capabilities** are additive interfaces in opt-in worlds (a minor,
no rebuilds). The **only** thing that forces a major is editing the shared
`types` / `callback-dispatch` enums — which is exactly what the columnar change
did (it removed the row-major batch arms). So the columnar work was taken as a
single coordinated major-4 break — core + codegen + a full catalog rebuild +
re-stamp + conformance — while there are no external consumers yet, rather than
breaking users later. Major-4 is again the frozen baseline; future growth is
additive minors off it.
