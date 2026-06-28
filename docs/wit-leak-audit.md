# v3 WIT leak audit (DuckDB-internal struct by-value leak check)

Date: 2026-06-28 (v3 stabilization). Scope: every interface in
`wit/duckdb-extension/`, with focus on the C++-backed (advanced-tier) ones.

## What a "leak" would be

A leak is any place a DuckDB-INTERNAL struct layout crosses the WIT boundary
by-value -- i.e. the component's marshalling becomes coupled to a DuckDB C++ type's
field order / discriminant values, so a DuckDB ABI change silently corrupts data
(the historical failure mode: the rich-types bump shifting enum discriminants).
The frozen contract is sound iff EVERYTHING crosses as a NEUTRAL type or via the
`complex(type-expr, json)` escape hatch, and the core shim does explicit mapping.

## Method

For each interface, enumerate the types it exchanges (`use types.{...}` + its own
records/variants/enums) and confirm each is one of:
- a neutral scalar / `duckvalue` / `duckerror` / `columndef` (the frozen `types`),
- the `complex(complexvalue{type-expr,json})` escape hatch,
- a flat descriptor of indices / neutral enums / strings / JSON the core builds.

## Result: CLEAN. No DuckDB-internal struct leaks by-value.

Every interface exchanges only the frozen neutral `types` plus flat, neutral
records of its own. The by-value crossings in the whole surface reduce to:
column indices (`u32`), neutral comparator enums, operator-type NAMES as strings,
JSON blobs, and `duckvalue`/`complexvalue`. No `LogicalType` internals, `Vector`,
`DataChunk`, `Expression`, `TableFilter`, `LogicalOperator`, or `PhysicalOperator`
ever crosses by value.

### Per-interface findings (advanced / C++-backed tier)

| Interface | Types crossed | Verdict |
|---|---|---|
| `types` | neutral scalars + `complexvalue{type-expr:string, json:string}` | The escape hatch is a FLAT record (no `duckvalue` ref -> no recursion); nested/future types ride it as text. The `logicaltype`/`duckvalue` discriminants are WIT-internal and decoupled from DuckDB type codes -- the core maps explicitly (DATE=13, TIME=14, ... non-contiguous) in `duckdb_type_to_logical`, never a raw cast. **No leak.** |
| `storage` / `storage-dispatch` / `storage-write-dispatch` | `scan-request`, `scan-filter{column:u32, op:compare-op, value:duckvalue}`, `compare-op{eq,ne,...}`, `columndef`, `resultset` | Pushdown is column index + neutral comparator + `duckvalue`, not a `TableFilter` tree. **No leak.** |
| `index` / `index-dispatch` / `index-write-dispatch` | `index-hit{rowid:s64, distance:f32}`, float vectors, `duckvalue` | Plain numbers + vectors. **No leak.** |
| `optimizer` / `optimizer-dispatch` (v3) | `plan-node{id:u32, op-type:string, parent, params-json:string}`, `plan-shape`, `rewrite-directive`, `structured-rewrite` | The plan crosses FLATTENED: `op-type` is the `LogicalOperatorType` NAME (a stable string), params are JSON -- NOT a by-value `LogicalOperator` tree. **No leak (by design).** |
| `parser` / `parser-dispatch` (v3) | `parse-outcome{declined, rewrite(string)}` | Text in, SQL text out. No parse tree. **No leak (by design).** |
| `table-stream-dispatch` (+ v3 filter) | `table-filter{column:u32, op:filter-op, values:list<duckvalue>}`, `filter-op{eq,...,is-in,is-null,...}` | Same neutral shape as `storage.scan-filter`. **No leak.** |
| `aggregate-incr-dispatch` (+ v3 window) | `window-frame{start:u64, end:u64}`, `rowbatch`, `duckvalue` | Integer frame bounds + neutral rows; no engine window-state struct. **No leak.** |
| `collation` / `compression` / `encoding` / `secret`(+dispatch) / `settings`(+dispatch) / `coordinate-system` / `lifecycle` / `conn-dispatch` / `file-*` / `copy-dispatch` / `catalog` / `macro-ext` / `types-ext` / `arrow-ext` | `duckerror`, strings, neutral records (`crs-def`, `secret-param`, `secret-kv`, `file-info`, `logical-type{name, physical:string}`, ...), `columndef` | All flat/neutral; `catalog.logical-type` carries the physical type as a STRING. Arrow crosses as encoded IPC bytes (`columndef` schema), not a `DataChunk`. **No leak.** |

## The one watched seam (documented, not a leak)

`types.logicaltype` / `types.duckvalue` discriminant ORDER is load-bearing for the
canonical ABI (appended-to only; never reordered -- see the freeze policy). It is
NOT tied to DuckDB's type codes: the core translates between the WIT discriminant
and the DuckDB type id explicitly. The risk is a future contributor reordering or
inserting a case (a MAJOR-forcing break), which the freeze policy rule #1 forbids
and `verify-catalog`'s digest check catches.

## Conclusion

The frozen v3 surface has no by-value DuckDB-internal struct leak. A DuckDB ABI
change can therefore be absorbed entirely in the core C++ shims (re-anchor the
explicit neutral<->internal mapping) without a WIT bump -- which is the invariant
the freeze depends on.
