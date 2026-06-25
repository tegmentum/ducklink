# Plan: function prefixes — SPARQL-style namespacing for DuckDB functions (ducklink)

## Status (2026-06-25)

Adapted from sqlink's `docs/plans/PLAN-prefixes.md` for ducklink's DuckDB-wasm
component architecture. Not yet started. No new WIT capability required — the
registration wrapper goes in the existing host forwarding path (the analog of
sqlink's loader-bridge wrapper). A new `prefix-dotcmd` extension joins the
dotcmd family (`core-dotcmd`, `bundle-dotcmd`).

## Motivation — ducklink has already lived this problem

The 178-component catalog makes SQL-function name collisions inevitable, and
ducklink has hit them repeatedly with ad-hoc, lossy workarounds:

- **`http` → `httpclient`, `sqlite` → `sqlitewasm`**: extensions renamed because
  the name collided with an embedded official extension the core resolved first.
- **The "no-overlap-with-builtins" rule**: every component author must manually
  check each function name against DuckDB's builtins before implementing.
- **The de-embed components register OFFICIAL names** — `jsonfns` registers
  `json_valid`/`json_extract`/…, `inetfns` registers `host`/`family`/…,
  `spatialfns` registers `ST_*`. These deliberately shadow the (now-removed)
  embedded versions, and **two such components could collide** (e.g. a second
  JSON impl). Today that's silent or an error, with no operator visibility.

Prefixes replace the ad-hoc renames with principled, SPARQL-style namespacing:
`jsonfns__json_valid(...)` is always available and unambiguous, while bare
`json_valid(...)` keeps working. The operator can SEE collisions and pin which
implementation wins the bare name.

## Naming

Same as sqlink: the feature is **prefixes**, not "namespaces" ("namespace" in
SQL already means a catalog/schema — `main`, `temp`, an ATTACHed db).

### Separator: `__` (double underscore)

Same choice and rationale as sqlink — DuckDB identifiers are
`[A-Za-z_][A-Za-z0-9_]*` unquoted, so `prefix__name(...)` needs no quoting,
unlike `:` (SPARQL-canonical, requires `"foaf:name"`) or `.` (collides with
DuckDB's `schema.function` / `catalog.schema.table` syntax). DuckDB's
case-insensitive identifier folding applies to both the prefix and the name.

## Architecture

### The expansion is required; the format is opaque

Identical to sqlink. Each prefix has `name` (short, used in SQL) + `expansion`
(an opaque global-identity token — a URL, `com.tegmentum.ducklink`, a URN, any
string). ducklink does NOT validate the expansion format. Function identity is
`(expansion, function_name, n_args)`, not the short prefix; the short prefix is
a per-database alias.

### THE KEY ADAPTATION — DuckDB does not silently last-wins-shadow

This is the one place ducklink diverges materially from the sqlink plan, and it
is a **prerequisite to resolve before implementing** (Sequencing step 0).

sqlink's whole conflict model rests on SQLite's `sqlite3_create_function`
silently replacing an earlier registration (last-wins). DuckDB is different:
- Scalar functions are registered into a **function set** keyed by name, with
  overloads distinguished by argument signature (`ScalarFunctionSet`).
- `duckdb_register_scalar_function` (the C API the core uses in
  `register_scalar_function_on_connection`, core/src/lib.rs:5716) **can return
  `DuckDBError` on a conflicting registration** rather than silently shadowing —
  the session already observed `duckdb_register_scalar_function failed for …`
  logging at line 5765.

So the bare-name-on-collision behavior cannot be "let SQLite shadow it." The
ducklink wrapper must EXPLICITLY decide and effect the bare-name owner. Two
viable mechanisms, to be settled in step 0:

- **(a) Drop-and-re-register.** On a bare-name collision, the wrapper
  unregisters the existing bare function (`CREATE OR REPLACE` semantics / a
  remove + register) and registers the new (or pinned) owner. The qualified
  forms of all colliding extensions remain registered independently.
- **(b) Overload coexistence.** If DuckDB tolerates two impls at the *same*
  name+signature via the function set (it generally does NOT — same signature is
  a conflict), this is moot; different signatures already coexist as overloads
  and aren't a "collision" in this plan's sense.

The plan assumes **(a)**: the wrapper owns bare-name registration explicitly,
re-registering the pinned/last/first owner as policy dictates, because DuckDB
won't do silent last-wins for us. This makes the operator pin
(`sqlite_sqlink_prefix_pin` analog) load-bearing rather than optional polish.

### Storage (user database, `__ducklink_prefix*` tables)

Per-db tables in the attached DuckDB database (travels with the `.duckdb` file),
mirroring sqlink's per-db rationale. DuckDB has no `sqlite_`-reserved hiding
convention, so use a `__ducklink_` name prefix (and these stay out of casual
`.tables` via the dotcmd's filter, as `__ducklink_loaded_extensions` already is):

```sql
CREATE TABLE __ducklink_prefix (
    name         VARCHAR PRIMARY KEY,   -- short prefix: 'foaf', 'tegmentum'
    expansion    VARCHAR NOT NULL,      -- opaque expanded form
    description  VARCHAR,
    created_at   BIGINT NOT NULL,
    last_used_at BIGINT
);
CREATE INDEX __ducklink_prefix_expansion ON __ducklink_prefix(expansion);

CREATE TABLE __ducklink_prefix_function (
    expansion      VARCHAR NOT NULL,    -- joins on expansion, NOT short name
    function_name  VARCHAR NOT NULL,    -- bare name, e.g. 'json_valid'
    extension_name VARCHAR,             -- which component registered it (audit)
    shape          VARCHAR NOT NULL,    -- scalar|table|aggregate|collation|...
    n_args         INTEGER,             -- arity (-1 variadic)
    registered_at  BIGINT NOT NULL,
    PRIMARY KEY (expansion, function_name, shape, n_args)
);

-- Operator pin: which expansion's impl wins the bare name on collision.
CREATE TABLE __ducklink_prefix_pin (
    function_name VARCHAR NOT NULL,
    shape         VARCHAR NOT NULL,
    n_args        INTEGER NOT NULL,
    expansion     VARCHAR NOT NULL,
    set_at        BIGINT NOT NULL,
    PRIMARY KEY (function_name, shape, n_args)
);
```

ducklink adds a `shape` column (vs sqlink) because ducklink has MORE
registration shapes than SQLite (scalar / table / aggregate / collation / pragma
/ cast / index / storage-backend / files-backend / macro). v1 covers the
function-like shapes; the rest are noted in v1 scope.

### Conflict resolution: bare name preserved, qualified forms additive

Same three cases as sqlink (no-collision → bare + qualified; collision → bare
follows policy + both qualified forms + warning + operator pin; unload → bare
reverts), with the **DuckDB twist** that "bare follows policy" is effected by the
wrapper's explicit drop-and-re-register (above), not by silent shadowing. The
hard constraint is identical: **existing SQL must not break** — `SELECT
json_valid(...)` keeps working exactly as before, regardless of how many
components load or collide.

### What this feature IS / IS NOT

Identical to sqlink: it adds always-available `prefix__name` qualified forms,
load-time collision warnings + a `.prefix conflicts` view, and an optional
`.prefix prefer` pin. It does NOT change which impl bare `name()` hits beyond the
operator pin, does NOT error on ambiguity at call time, and does NOT require
users to update existing SQL. Strictly additive.

### Registration flow (in the host forwarding wrapper)

ducklink's registration path (the analog of sqlink's loader-bridge wrapper):
a component calls `runtime.register-scalar/table/aggregate` → captured in
`ducklink-runtime` (`HostScalarRegistry::register` → `PendingScalar`) → the host
drains + forwards (`convert_pending_scalar_registration`, ducklink-host) → the
core registers it (`register_scalar_function_on_connection`,
`duckdb_register_scalar_function`). The prefix wrapper sits at the host
forwarding step (it has the extension name, the function name, the arity, and
the callback handle):

1. Read the component's `(preferred_prefix, expansion)` from its **registry
   entry** (the manifest analog — new `prefix`/`expansion` fields in
   registry/index.json). If absent → deprecation fallback: prefix = the
   extension name, expansion = `ducklink-internal://<extension>` + warn (hard
   error after v1.1).
2. Resolve the prefix in `__ducklink_prefix` (insert / matching-expansion-reuse
   / numbered-fallback `foaf2` on a different-expansion clash + warn).
3. Insert into `__ducklink_prefix_function` keyed by `(expansion,
   function_name, shape, n_args)`.
4. **Always** register the function with the core under `prefix__function_name`
   (a second `register_*_function_on_connection` call with the qualified name
   pointing at the SAME callback handle — the host already maps name→callback,
   so this is one extra register).
5. **Bare-name registration** under `function_name` — but because DuckDB may
   reject a duplicate, the wrapper drops any existing bare registration for the
   same `(name, shape, n_args)` first (or uses CREATE-OR-REPLACE-equivalent),
   then registers the new/pinned owner.
6. **Pin override**: if `__ducklink_prefix_pin` pins a different expansion for
   `(name, shape, n_args)`, re-register the bare name against the pinned
   expansion's callback so the pin survives load order.
7. **Collision logging**: if step 3 found a prior row for the same
   `(name, shape, n_args)` from a different expansion, warn — naming all
   colliding extensions, the current bare owner, and the qualified forms.

The host's existing name→callback dispatch (the runtime callback registry) does
the call-time work; the wrapper never intercepts call-time dispatch.

## Surface

### Dot commands (in `prefix-dotcmd` extension)

Modeled on `extensions/core-dotcmd` + the new `extensions/bundle-dotcmd` (the
dotcmd `registry` + `spi` WIT in `wit/dotcmd/world.wit`; `spi.query` runs SQL on
the live connection). Same command set as sqlink:

```
.prefix add foaf http://xmlns.com/foaf/0.1/ "Friend of a friend"
.prefix add tegmentum com.tegmentum.ducklink
.prefix list                      -- name | expansion | description | last_used
.prefix functions foaf            -- functions under foaf's expansion
.prefix expansion foaf
.prefix rename foaf bar
.prefix modify foaf "Updated description"
.prefix delete foaf               -- alias row only; expansion-keyed functions persist
.prefix prefer json_valid jsonfns -- pin bare-name owner; writes _prefix_pin
.prefix unprefer json_valid
.prefix conflicts                 -- current bare-name ambiguities + owner + pin
.prefix verify                    -- stale-entry check vs loaded components
```

All commands go through `spi.query` against the user-db prefix tables (read-only
for list/functions/expansion/conflicts/verify; writes for add/delete/rename/
modify/prefer/unprefer).

### Component manifest declaration (registry/index.json)

Add two fields to the registry entry schema:
```json
{
  "name": "jsonfns",
  "prefix": "jsonfns",
  "expansion": "com.tegmentum.ducklink.json",
  "...": "..."
}
```
Both required for new components; the loader rejects missing both after v1.1.
v1 deprecation fallback: prefix = the extension name, expansion =
`ducklink-internal://<name>` + a load-time warning. (A component MAY also carry
the pair at runtime via a `Loadresult`/`Funcopts` field, but the registry is the
canonical manifest home — it's where per-extension metadata already lives and
where `tooling/verify-catalog.py` can enforce presence.)

### Function call resolution

Same table as sqlink: `foaf__name(...)` always works if registered; bare
`name(...)` works (unique → the one impl; collision → policy/pin owner, warning
logged); `unknown__name(...)` → DuckDB's "Catalog Error: Scalar Function with
name unknown__name does not exist".

## Capability requirements

No new WIT capability. `.prefix` reads/writes the user db via the dotcmd `spi`
(already exists). The registration wrapper runs host-side during load (the
existing forwarding path). A `Capability::PrefixRegistry` was considered and
rejected, exactly as in sqlink — spi suffices.

## v1 scope

- The three `__ducklink_prefix*` tables + migration (created on first `.prefix`
  use / first prefixed registration).
- The host registration wrapper around **scalar + table + aggregate**
  registration (the three function-like shapes), with the DuckDB
  drop-and-re-register bare-name mechanism resolved in step 0.
- `prefix-dotcmd` with the dot-commands above.
- `prefix`/`expansion` fields in registry/index.json + verify-catalog
  enforcement (warn in v1) + the deprecation fallback.
- Tests: bare-name happy path, collision path, qualified-form availability,
  rename/delete-with-shared-expansion, pin/unpin, missing-manifest deprecation
  warning, all three shapes.
- A `BUILDS.md`-style note + README section.

## Out of scope (v2+)

Same as sqlink, plus ducklink-specific items:
- **The other registration shapes** (collation / pragma / cast / custom-index /
  storage-backend / files-backend / macro) — namespace them in v1.1+ once the
  scalar/table/aggregate pattern is proven. (sqlink's "all four shapes in v1"
  becomes "three function-like shapes in v1" here because ducklink has ~10
  shapes, several of which — storage/files/index — are keyed by an ATTACH TYPE /
  protocol, a different collision surface.)
- Per-query prefix overrides; prefix lock-in; prefix-scoped permissions; bulk
  import/export; typo auto-suggestion; cross-database sync; a hosted
  prefix/expansion registry. All as in sqlink.

## Dependencies

- The dotcmd `spi.query` (exists — `core-dotcmd`/`bundle-dotcmd` use it).
- `runtime.register-scalar/table/aggregate` + the host forwarding path (exists;
  this plan wraps it).
- The registry/index.json manifest + `tooling/verify-catalog.py` (exists).
- **Prereq (step 0): characterize DuckDB's duplicate-registration behavior** —
  does `duckdb_register_scalar_function` error / overload / replace on a same
  name+signature? This decides the bare-name mechanism (drop-and-re-register vs
  other). Verify against the core (core/src/lib.rs:5716) with a 2-component
  same-name probe before writing the wrapper.

## Sequencing

0. **Resolve DuckDB collision semantics** (the prereq above). Small probe; gates
   the wrapper design.
1. **Schema + migration** (`__ducklink_prefix*`), applied on first use.
2. **Manifest fields** (`prefix`/`expansion` in registry/index.json) + the
   deprecation-fallback + verify-catalog awareness.
3. **Registration wrapper** in ducklink-host's pending-* forwarding: insert/query
   the tables, register bare + `prefix__name`, drop-and-re-register the bare
   owner per policy/pin, collision-warn.
4. **`prefix-dotcmd`** with the dot-commands.
5. **Native + smoke tests** (happy path + collision + rename + pin, scalar/table/
   aggregate).
6. **In-tree component audit**: scan all 178 registry entries, propose
   `(prefix, expansion)` for each, warn on missing — drive the migration before
   the v1.1 hard-error cutover. The de-embed components (jsonfns/inetfns/
   spatialfns/…) are the priority since they intentionally register official
   names.

## Resolved design decisions (inherited from sqlink, ducklink-adjusted)

1. **Prefix-collision auto-fallback (Q1).** Numbered alternative (`foaf2`) +
   warn — same as sqlink.
2. **`last_used_at` policy (Q2).** Updated only on operator `.prefix` commands,
   NOT on function dispatch — same as sqlink (zero per-call overhead).
3. **Deprecation window (Q3).** Tied to ducklink v1.1, not calendar; v1 ships
   the synthetic-expansion fallback + warning, v1.1 makes it a hard load error.
4. **Function-shape coverage (Q4).** v1 = scalar + table + aggregate (the
   function-like shapes). The de-embed-driven collisions live entirely in these
   shapes. The ATTACH/protocol-keyed shapes (storage/files/index) and
   collation/pragma/cast/macro defer to v1.1+ — they have a different collision
   surface (TYPE names / protocol prefixes) that may want different semantics.
5. **Backwards compatibility (Q5, hard constraint).** Bare names preserve
   current behavior; qualified forms are purely additive — same as sqlink. The
   ducklink delta: "preserve current behavior" is effected by the wrapper's
   explicit bare-name ownership (DuckDB won't silently last-wins), backed by the
   `__ducklink_prefix_pin` table.

## References

- sqlink `docs/plans/PLAN-prefixes.md` (the source plan this adapts).
- SPARQL 1.1 §4 (prefixed names).
- DuckDB function registration: `duckdb_register_scalar_function` /
  `ScalarFunctionSet` overloading (the C API the core uses,
  `../duckdb-wasm/core/src/lib.rs:5716`).
- ducklink's collision history: the `http`→`httpclient`, `sqlite`→`sqlitewasm`
  renames; the no-overlap-with-builtins rule; the de-embed components registering
  official names (memory: `component-extension-catalog`, `lean-core-deembed`).
