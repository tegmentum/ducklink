# v4 Columnar Dispatch ABI — deep performance pass

Status: **prototype proven, GO recommended.** This is the design + GO/NO-GO for
the proposed `duckdb:extension@4.0.0` columnar dispatch contract. It is the
centerpiece of the perf pass that PRECEDES #183 (native parity), so the native
extension targets the FINAL ABI.

## 1. Hot-spot map (where the time goes)

The component-extension scalar path crosses the WIT canonical ABI per DataChunk.
Measured costs (Apple silicon, release; the full chain for a 1M-row i64 scalar):

| Stage | cost | where | status |
|---|---|---|---|
| wasmtime compile of the core (~96 MB) | ~8 s cold | first invocation | LANDED: disk cache (~10x warm) + precompile `.cwasm` |
| per-call WIT invocation overhead | ~0.74 us/row (pre-batch) | host<->ext | LANDED: batched dispatch (one call/chunk) |
| **row-major value marshalling at the WIT boundary** | **~73 ns/row** | core<->host<->ext canonical ABI | **THIS PASS: columnar** |
| guest neutral conversion (duckvalue->NeutralValue) | ~24 ns/row | guest shim | LANDED: scratch reuse (68.99 -> 23.81 ns/row) |
| core per-cell DuckDB vector read/write | per-cell FFI | core | LANDED: per-column pointer+validity hoist; columnar removes the loop entirely |

Every row-major-COMPATIBLE lever is already landed (compile cache, precompile,
batched dispatch, guest scratch reuse, `Arc<str>` handle, core per-cell FFI
hoist, TVM copy elision). What remains — bulk SIMD transfer, per-column instead
of per-cell FFI, elimination of the `Vec<Vec<duckvalue>>` rowbatch allocation —
is **gated by the columnar ABI**. The row-major marshalling (~73 ns/row) is now
the single largest item in the dispatch path.

## 2. The lever: columnar / typed-column ABI

Today the batch ABI is row-major tagged enums:
`call-scalar-batch(rows: list<list<duckvalue>>) -> list<duckvalue>`. `duckvalue`
is a ~24-byte tagged variant, so the boundary serializes a tag + payload PER
CELL and reallocates a `list<>` PER ROW — fundamentally not SIMD-able, O(rows x
cols) with branchy per-cell work.

DuckDB is a VECTORIZED engine: its vectors are already flat contiguous arrays.
The columnar ABI passes each argument as a **typed column** (a contiguous
`list<s64>`/`list<f64>`/... buffer) plus a packed **validity bitmap**:

```
call-scalar-batch-col(handle, args: list<colvec>, ctx) -> result<colvec, duckerror>
```

Fixed-width columns lower/lift as a single bulk `memcpy`; the core fills them by
`memcpy` straight from `duckdb_vector_get_data` (no per-cell read loop) and the
validity is `duckdb_vector_get_validity` copied verbatim. Var-width arms
(`text`/`blob`) and the `complex` escape hatch stay element-wise — identical to
today for those types. NULL is carried out-of-band in the bitmap, so NULL
semantics are byte-identical to the row-major `duckvalue.null` arm.

WIT design: [`wit/v4-columnar-draft/column-types.wit`](../wit/v4-columnar-draft/column-types.wit)
and [`callback-dispatch-col.wit`](../wit/v4-columnar-draft/callback-dispatch-col.wit).

## 3. Prototype benchmark + quantified win (GO/NO-GO)

A real `cargo component` guest + real `wasmtime` 46 host, both doing the
identical `+1` i64 scalar over 1M rows in 2048-row chunks, checksums asserted
equal. [`benches/columnar-abi-prototype`](../benches/columnar-abi-prototype).

```
ROW-MAJOR  list<list<duckvalue>>  :  1891.94 ms     94.65 ns/row
COLUMNAR   list<colvec>           :    17.09 ms      0.85 ns/row
speedup: 110.72x   latency reduction: 99.1%

boundary-only (inputs prebuilt; isolates the canonical-ABI crossing):
ROW-MAJOR  :   73.48 ns/row
COLUMNAR   :    0.89 ns/row     speedup: 82.58x
```

**Recommendation: GO.** The marshalling boundary drops ~82-110x. This is the
churn window (no users yet); the same change after users would be far costlier.

### Projected total scalar-path gain

The full component scalar pre-columnar costs ~73 (boundary) + ~24 (guest
neutral) + core per-cell read/write. Columnar collapses the boundary to ~1
ns/row AND turns the core read/write + guest conversion into bulk ops, and
unlocks SIMD in the guest kernel. For a trivial scalar the dispatch overhead
goes from ~100 ns/row to a few ns/row (order-of-magnitude). For real components
the per-row guest WORK still dominates, but the dispatch TAX they pay shrinks to
near-zero and the inputs arrive as SIMD-ready slices.

## 4. Coordinated major-4 bump (the plan, review-gated)

Major-4 is a TRUE break: it removes the row-major batch entries and bumps
`CONTRACT_MAJOR 3 -> 4`. Acceptable now (no users). Coordinated steps:

1. **WIT**: adopt `column-types.wit` + `callback-dispatch-col.wit` into the live
   world at `@4.0.0` across the tree; new canonical digest = new contract
   identity; host REJECTS `@3.x` by design.
2. **Core** (`duckdb-wasm/core/src/lib.rs`): `execute_scalar_function` builds
   `colvec`s by `memcpy` from each `duckdb_vector_get_data` + validity, calls
   `call-scalar-batch-col`, and writes the result column back to the output
   vector by `memcpy`. Removes the `rows: Vec<Vec<Duckvalue>>` build + the
   per-cell `read_scalar_argument`/`write_duckvalue_to_vector` loop. (See
   `duckdb-wasm/docs/v4-columnar-core-path.md`.)
3. **Codegen** (`datalink-extcore`): generated `duckdb` shims call
   `scalar_batch_col` (landed primitive) instead of `scalar_batch`; cores need
   ZERO changes (the bridge runs the same per-row neutral `dispatch`; a core may
   later add a true vectorized kernel but is never required to).
4. **Rebuild + re-stamp + conformance**: rebuild ~190 catalog components @4.0.0,
   re-stamp the catalog digest, re-record conformance, prove one representative
   component end-to-end on the @4 host. THIS is the heavy, review-gated step;
   not done in the perf-pass branch.

## 5. Landed vs deferred

### Phase 1 — the @4.0.0 foundation (LANDED, branch `feat/wit-4.0.0`)

- **@4.0.0 LIVE contract.** `column-types.wit` + the columnar `callback-dispatch`
  (call-scalar-batch-col / call-aggregate-col / call-cast-col; cold singletons
  row-major) are in the live world. `CONTRACT_MAJOR = 4`, `CONTRACT_MINOR = 0`,
  `CONTRACT_VERSION = 4.0.0`, propagated across the tree (2397 WIT files).
  New canonical digest **`a2ad9764ac971345d6a650b92edbda034b160980acf148d354126f7e6f92ba40`**
  (43 canonical files), confirmed emitted by `ducklink-runtime/build.rs`.
- **Codegen columnar emission.** `datalink-extcore`'s `duckdb_shim!` /
  `duckdb_agg_shim!` emit the columnar dispatch (colvec<->NeutralColVec bridge to
  the per-row neutral `dispatch`/`dispatch_aggregate`) — ZERO per-core changes.
  Verified: all **42 macro-based catalog components** (scalar + aggregate) build
  @4.0.0 and export call-scalar-batch-col / call-aggregate-col / call-cast-col;
  `datalink-extcore` columnar tests green.
- **Core memcpy dispatch (source).** `duckdb-wasm` core `execute_scalar_function`
  / `execute_cast` / aggregate finalize build colvecs by bulk memcpy from
  `duckdb_vector_get_data` (+ validity), cross via the columnar funcs, and write
  back by memcpy (`build_colvec` / `write_colvec_to_vector` /
  `aggregate_rows_to_colvecs`). Columnar core bindings regenerated.
- **Native runtime engine (`ducklink-runtime`) columnar dispatch** wired
  (rows<->colvec at the wasmtime boundary); compiles; tests green incl. the new
  `major_4_rejects_frozen_3_0_0_components` break proof (@3.x rejected by design).

### Phase 2 — coordinated rebuild (DEFERRED, review-gated)

- **The 168 hand-written components.** They predate the codegen and put logic in
  varying methods (some in `call_scalar`, some in `call_scalar_batch`), so they
  need per-component columnar migration (or pull-up onto `duckdb_shim!`) — NOT a
  safe blind codemod. Catalog re-stamp (`gen-catalog`) + conformance + verify
  follow once all artifacts are rebuilt.
- **The wasm core build + `ducklink-host` build are BLOCKED by a PRE-EXISTING
  (columnar-independent) drift**: `core/src/lib.rs` and `ducklink-host` reference
  host interfaces (`storage-host`, `optimizer-host`, `parser-host`,
  `table-stream-host`, `collation-host`, `files-host`, `pragma-host`) that are
  absent from the trimmed core WIT on this branch (present in the stale committed
  bindings only). These host-interface WIT files must be restored to the core
  world before the core/host compile — the core-side half of the coordinated
  rebuild. End-to-end conformance/e2e through native DuckDB depends on it.
- **SIMD vectorized kernels per core** (now POSSIBLE on the contiguous buffers) —
  opt-in per core after the ABI lands.

Non-ABI levers are already exhausted (see section 1); columnar is the gateway to
the rest.
