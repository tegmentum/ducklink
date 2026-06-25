# Plan — remaining WIT capability surfaces

> **STATUS (complete):** all 5 items implemented + verified in-sandbox on the
> lean core; full smoke 171/171. Item 1 (rich types) was the one contract bump
> (forced a full-catalog rebuild — the canonical-ABI finding); Items 2/4/3/5 are
> additive new interfaces. Item 3's optimizer auto-rewrite (the planner choosing
> a wasm component's HNSW index) was the keystone. Item 5 needed no core change
> (register-cast was already wired). Deferred follow-ons: decimal/interval/uuid +
> nested types (later type bump), GDAL/PROJ + R-tree (engineering on proven
> capabilities), the full vss-style hnsw_index_scan TableFunction rewrite.

The de-embed program has delivered every official extension whose surface fits
the *existing* WIT capabilities (scalar / table / aggregate / cast / macro /
pragma / catalog / files). What remains needs **new capability surfaces**. This
plan specifies each one. They are the deliverables that decide the final
`duckdb:extension` contract before it ships.

Everything here reuses two proven techniques:
- **The C++ shim pattern** (`core/cpp/wasm_storage.cpp`, `wasm_files.cpp`): a C++
  TU compiled into the wasm core with `sqlite_scanner`'s flags, subclassing a
  DuckDB abstract class and forwarding to a Rust C-ABI bridge → a host WIT import
  → the component's dispatch export. Registered **mid-session** via the
  dynamic-registration pull (`storage-list-types` style).
- **The C-lib-in-wasm build** (rusqlite-bundled; the prebuilt `libduckdb-wasi.a`,
  `libpq.a`, GEOS/PROJ/GDAL): a native library compiled to `wasm32-wasip2` and
  linked into a component behind the WIT boundary.

Ordering is by leverage and dependency. Items 1–2 are foundational and unblock
the rest.

---

## Item 1 — Richer logical types (foundational)

**Gap.** `types.wit`'s `logicaltype` enum is only `boolean / int64 / uint64 /
float64 / text / blob`, and `duckvalue` matches. Every component today flattens
real types to text (dates, timestamps, decimals, lists). This caps data fidelity
for *all* components and blocks TIMESTAMPTZ (icu), DECIMAL, and nested types.

**Surface (WIT).** Extend `logicaltype` + `duckvalue` with: `int8 int16 int32`,
`uint8 uint16 uint32`, `date`, `time`, `timestamp`, `timestamptz`, `interval`,
`decimal(width, scale)`, `uuid`, and the nested `list<duckvalue>` /
`struct(list<field>)` (a follow-on; scalars first). Each new `logicaltype`
variant needs a matching `duckvalue` variant (e.g. `timestamp(s64 micros)`,
`decimal(record { value: s128-as-2xu64, width: u8, scale: u8 })`).

**Work.** Purely additive + mechanical, but touches every conversion site:
`convert_extension_logicaltype` / `convert_*_duckvalue` (both directions) in
`ducklink-runtime` + `ducklink-host`, `write_duckvalue_to_vector` and the
`duckdb_value -> duckvalue` reads in the core (`core/src/lib.rs`), and
`neutral_logicaltype_to_core`. No C++ shim. TIMESTAMPTZ additionally needs the
session timezone honored at bind (the core already links icu-less; tz math is the
one piece needing binder awareness — scope a `timestamptz` that round-trips
micros + relies on the session TZ default).

**Verification.** A component scalar returning a real `DATE`/`TIMESTAMP`/`DECIMAL`
shows the correct type via `typeof(...)` and value; `icufns` (or a new tz
component) does `now_tz('UTC')`-style returns.

**Effort.** Medium (broad but no new architecture). **Unblocks:** icu tz, GEOMETRY
(Item 5), and higher fidelity everywhere. **Do first.**

---

## Item 2 — Collation registration (icu `COLLATE`)

**Gap.** Real `ORDER BY x COLLATE de` needs DuckDB to know a *collation*, not a
scalar. The WIT has no collation hook; `icufns` ships `icu_sort_key` as a
workaround.

**DuckDB surface.** A collation is a `ScalarFunction` (text → sort-key text)
registered via `ExtensionLoader`/`Catalog` `CreateCollation` (an
`CreateCollationInfo` with the function + `combinable`/`not_required_for_equality`
flags). We already have the **scalar callback** dispatch — a collation is that
callback registered through the collation path.

**Surface (WIT).** A small `collation` interface: `register-collation(name:
string, callback: scalar-callback, combinable: bool)`. Reuses the existing
`scalar-callback` resource and `call-scalar` dispatch — no new dispatch needed.

**Work.** Host/runtime: capture `register-collation` (mirror the scalar
registry). Core: a tiny C++ or C-API path that registers a collation whose
transform calls back to the component's scalar callback (the collation transform
== the existing scalar invoke). Likely a small C++ helper (`wasm_collation.cpp`)
OR reuse the scalar registration + a `collation` flag if the C API exposes it.
`icufns` then registers `icu_sort_key` as collations (`icu_de`, `icu_sv`, …).

**Verification.** `CREATE TABLE t(x VARCHAR COLLATE icu_sv); ... ORDER BY x` puts
`ä` after `z` for Swedish — vs default byte order. No live server needed.

**Effort.** Small–Medium (reuses scalar dispatch). Tractable; good second.

---

## Item 3 — Custom index (vss HNSW + spatial R-tree) — the keystone

**Gap.** `CREATE INDEX ... USING HNSW`/R-tree needs a custom index type. No WIT
index surface exists. This is the single highest-leverage item — it serves **both**
vss and spatial's index.

**DuckDB surface (two parts).**
1. **Index type registration + lifecycle:** a `BoundIndex` subclass (bind from
   `CREATE INDEX`, `Append`/`Insert` to build, `Serialize`/`Deserialize` to
   persist into the DB block manager, `Delete`, `Vacuum`), registered via the
   index-type registry (`IndexType` with a `create_instance` callback in
   `DBConfig`/`ExtensionUtil::RegisterIndexType`).
2. **Optimizer integration (the hard half):** the planner must *choose* the index.
   vss adds an **optimizer-extension rule** matching `ORDER BY
   array_distance(col, q) LIMIT k` → an HNSW index scan; spatial matches
   `WHERE ST_Intersects(col, bbox)` → R-tree scan. So this needs the index WIT
   **plus** a planner-rule hook (an `OptimizerExtension` shim).

**Surface (WIT).** An `index` interface (component → core, register) +
`index-dispatch` (core → component, callbacks): `index-bind(create-info) ->
index-handle`, `index-append(handle, rowids, keys)`, `index-scan(handle,
query-key, k | bbox) -> list<rowid>`, `index-serialize(handle) -> blob`,
`index-deserialize(blob) -> handle`, `index-delete(handle, rowids)`. The C++
`WasmIndex : BoundIndex` shim forwards each to the bridge (same structure as
`WasmCatalog`). The optimizer rule can be a fixed C++ `WasmIndexOptimizer` that
recognizes the registered index's "trigger" pattern (declared by the component:
e.g. `{trigger-function: "array_distance", shape: top-k}`).

**Component side.** vss: the `hnsw_rs` or `usearch` Rust crate (HNSW). spatial:
the `rstar` Rust crate (R-tree). Both pure-Rust → wasm. Serialize the index to a
blob for persistence.

**Verification.** `CREATE TABLE v(id INT, e FLOAT[3]); CREATE INDEX h ON v USING
HNSW(e); ... ORDER BY array_distance(e, [..]) LIMIT 3` returns the nearest rows
and `EXPLAIN` shows the index scan. No live server.

**Effort.** Large — the lifecycle/persistence is tractable (mirrors storage); the
**optimizer-rule integration is the genuinely hard, novel part** and the main
research output of this item. **Unblocks:** vss HNSW and spatial R-tree.

---

## Item 4 — PRAGMA that generates SQL (fts `create_fts_index`)

**Gap.** `PRAGMA create_fts_index(table, id, cols...)` builds an inverted-index
schema (tables) + a per-table `match_bm25` macro. The WIT has a `pragma`
capability, but a sandboxed component can't run `CREATE TABLE`/`CREATE MACRO` on
the connection.

**Surface (WIT).** A host import `spi.run-sql(sql: string) -> result<_, string>`
the component can call from its pragma callback (the host already runs SQL on the
live connection for dot-commands — `DotcmdState::spi::query`). The component's
`create_fts_index` pragma generates the schema+macro SQL (using `ftsfns`'
tokenize/stem/bm25) and runs it via `spi.run-sql`.

**Work.** Add the `spi` import to the component world + host wiring (reuse the
existing dotcmd spi path). `ftsfns` adds the `create_fts_index` pragma that emits
the inverted-index DDL + a `match_bm25` macro referencing `bm25_score`.

**Verification.** `PRAGMA create_fts_index('docs','id','body'); SELECT * FROM docs
WHERE match_bm25(id, 'query') IS NOT NULL ORDER BY ...`. No live server.

**Effort.** Medium. Independent of Items 1–3. The `spi.run-sql` import is broadly
useful (any component that wants to create helper objects).

---

## Item 5 — GEOMETRY type + GDAL/PROJ (spatial full parity)

**Gap.** `spatialfns` uses WKT text; real spatial wants a first-class `GEOMETRY`
type, GDAL/PROJ-backed I/O + reprojection, and the R-tree index (Item 3).

**Work.** (a) A custom **logical type** registration that actually wires
`register-logical-type` (currently a stub) to a real DuckDB type +
binary-physical representation — depends on Item 1's type machinery. (b) Compile
GEOS/PROJ/GDAL to wasm (already done for embedded spatial — reuse the
`<ext>-deps.cmake` libs) and link them into a `geometryfns` component via the
C-lib-in-wasm pattern, exposing the full `ST_*` surface over the GEOMETRY type.
(c) The R-tree index via Item 3.

**Verification.** `GEOMETRY` columns, `ST_Transform` reprojection, an R-tree
spatial join.

**Effort.** Largest. Depends on Items 1 + 3. Lowest priority (the `spatialfns`
subset already covers common analytics).

---

## Sequence

1. **Item 1 (types)** — foundational; lifts fidelity everywhere and unblocks 2/5.
2. **Item 2 (collation)** — small, self-contained, finishes icu.
3. **Item 3 (custom index)** — keystone; the optimizer-rule integration is the
   core research deliverable; unblocks vss + spatial index.
4. **Item 4 (pragma + spi.run-sql)** — independent; finishes fts.
5. **Item 5 (GEOMETRY + GDAL)** — heaviest; needs 1 + 3; finishes spatial.

Each follows the validated arc: define the WIT surface → C++ shim (where a DuckDB
abstract class is involved) + Rust bridge → host/runtime routing → dynamic
registration → a component implementing it → verify in-sandbox, de-risking with a
stub milestone (M1) before the real build (M2), exactly as storage and files did.

## What this surface buys

When Items 1–4 land, the `duckdb:extension` WIT world will expose: scalar, table,
aggregate, cast, macro, **collation**, pragma + **spi.run-sql**, catalog (ATTACH),
files (virtual FileSystem), and **index** capabilities, over a **rich type
system** — enough for *any* DuckDB extension to ship as a portable, version-
independent wasm component. That completeness is the deliverable.
