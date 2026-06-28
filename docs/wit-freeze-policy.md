# The v3 WIT freeze policy

Status: **FROZEN at `duckdb:extension@2.3.0` (the "v3" surface), 2026-06-28.**

This document is the contract for evolving the `duckdb:extension` WIT after the v3
stabilization. It exists because the 2.0 -> 2.1 -> 2.2 -> 2.3 bumps were churn, and
the whole point of v3 was to complete the capability surface ONCE and then stop
moving it. Read this before touching anything under `wit/duckdb-extension/`.

## What "v3" is (and is not)

- **v3 is the FROZEN-SURFACE MILESTONE**, not a wasmtime semver major. It is the
  completed `duckdb:extension` capability surface, identified by its
  **content-addressed contract digest** (`witcanon:1` over the canonical WIT
  bytes; see `crates/ducklink-runtime/build.rs` and `CONTRACT_DIGEST`). The digest
  IS the contract identity (it changes iff the shape changes); the semver string is
  a human label and a coarse runtime proxy.
- **The WIT package stays at major 2** (`duckdb:extension@2.3.x`). `CONTRACT_MAJOR`
  is deliberately HELD at 2 and **will never bump again**. A major bump would
  reject every already-shipped `@2.x` component (a mass rebuild -- the exact churn
  v3 exists to end) and break the additive-only guarantee below. v3 was landed as
  an ADDITIVE MINOR (2.2 -> 2.3) so all ~189 existing components keep loading
  un-rebuilt (minor forward-compat: a `@2.k` component loads on a `@2.minor` host
  for `k <= minor`).

## The freeze rules (in priority order)

1. **NEVER edit the shared types or runtime enums.** The members of
   `types.duckvalue`, `types.logicaltype`, `types.capabilitykind`, and the
   `runtime.capability` variant are FROZEN. Adding/removing/reordering a case there
   is the ONLY thing that forces a contract MAJOR (the wasmtime component linker
   rejects import-subtyping: a host providing a superset enum cannot satisfy a
   component that imported the smaller one). Do not do it.

2. **New TYPES ride the escape hatch -- no bump at all.** Any new logical type or
   value (nested list/struct, future scalar types, custom types) crosses the
   boundary as `complex(complexvalue{ type-expr, json })`. The core reconstructs
   the real vector from the type-expression + JSON via the DuckDB C vector API
   (which has no recursion limit). This needs NO new interface and NO version bump.
   Reach for a new dedicated type case only if you are willing to pay a MAJOR --
   you almost never are.

3. **New CAPABILITIES are additive interfaces in opt-in worlds -- a MINOR bump,
   no rebuilds.** A genuinely new dispatch shape (something `complex()` cannot
   express) is a NEW `*.wit` interface plus a NEW `duckdb-extension-<cap>` world
   that a capable component opts into. Components that do not import it never
   rebuild and keep loading. This is how parser / optimizer / window / table-fn
   filter pushdown were added in v3, and it is the only sanctioned growth path.
   Bump the MINOR (`CONTRACT_MINOR`) in lockstep; never the MAJOR.

4. **Within an EXISTING interface, only ADD functions/records -- never change a
   signature.** Filter pushdown was added to `table-stream-dispatch` as a new
   `call-table-open-filtered` function (the original `call-table-open` stayed byte-
   frozen); the window variant was added to `aggregate-incr-dispatch` as a new
   `call-aggregate-window`. Editing an existing function's signature is a breaking
   change -- treat it as forbidden.

5. **The WIT version is DECOUPLED from the DuckDB version.** A routine DuckDB bump
   (e.g. 1.5.4 -> 1.6) is a CORE-SHIM re-anchor at the SAME WIT version -- never a
   contract bump. The DuckDB ABI churn is confined to the wasm core C++ shims
   (`duckdb-wasm/core/cpp/wasm_*.cpp`), which bind DuckDB's internal C++ headers.
   It does NOT reach the WIT or the components (they target the WIT world, are
   version-independent, and survived the 1.4 -> 1.5.4 retarget un-rebuilt). The
   only WIT-relevant rule a DuckDB bump must respect: **no DuckDB-internal struct
   may leak by-value into the WIT** -- everything crosses as a neutral type or via
   `complex(type-expr, json)`. (Leak audit: `docs/wit-leak-audit.md`.)

## Contract identity = the canonical-WIT hash (compose:dynlink)

The authoritative contract identity is the `witcanon:1` digest of
`wit/duckdb-extension/*.wit`, byte-compatible with
`compose-core::blobs::compute_wit_digest` in the webassembly-component-
orchestration framework (`compose:dynlink`). A shape change IS a hash change, so
the hash SUBSUMES the hand-rolled semver guard: the `CONTRACT_MAJOR`/`.minor`
runtime check (`check_component_contract`) stays only as the friendly-message
front door (it can introspect a loaded component's imported `@MAJOR.minor` but
cannot recompute its digest). The digest is recorded per registry entry
(`wit_contract`) and enforced at `tooling/verify-catalog.py` time. Do not build a
second, parallel contract guard.

## The bump procedure (when rule 3/4 genuinely applies)

This is the "2.1/2.2 procedure" -- additive, no mass rebuild:

1. Add the new `*.wit` interface(s) to `wit/duckdb-extension/` (canonical) and copy
   into `crates/ducklink-runtime/wit/deps/duckdb-extension/` (the host's dep copy).
2. Add the opt-in world(s) to `crates/ducklink-runtime/wit/duckdb-extension-host.wit`
   and, for component authors, document the world to opt into. Add the registration
   interface to the base world imports only if components declare via it in load().
3. Bump `CONTRACT_VERSION` in `tooling/propagate-wit.py`, run it (rewrites every
   managed WIT file's `@version`), and bump `CONTRACT_MINOR` + `CONTRACT_VERSION`
   in `crates/ducklink-runtime/src/lib.rs` (leave `CONTRACT_MAJOR = 2`). Bump the
   `types@<ver>` keys in the lib.rs `bindgen!{ with: ... }` maps.
4. Wire the host: a `pending_*` buffer + `reg::*Reg` struct + a `Host` impl that
   captures the registration + a `take_pending_*` drain + an `add_to_linker` line +
   (for a dispatch world) a `bindgen!` module. Drain into the core/native shim.
5. Re-stamp: `python3 tooling/gen-catalog.py` (recomputes `wit_contract`/version per
   entry); re-record conformance; `python3 tooling/verify-catalog.py`.
6. Confirm an existing un-importing component (e.g. geohash) still loads un-rebuilt.

## Out of scope (do not add a hollow interface)

- **Operator extensions** are infeasible by-value: DuckDB's `PhysicalOperator` /
  `OperatorExtension` is a recursive, engine-internal pipeline object that cannot
  cross the boundary (the same recursion wall as a by-value LogicalOperator tree).
  Steer custom physical execution to a TABLE FUNCTION (streaming + projection +
  filter pushdown via `table-stream-dispatch`). No operator interface ships.
- **By-value bound parse trees / LogicalOperator trees.** Parser and optimizer
  extensions are constrained to text/flattened-descriptor in, rewrite-directive out
  (string->SQL rewrite; flattened plan-shape -> rewrite directive). This is a
  permanent design limit of the by-value WIT boundary, not a TODO.

## The C++-only tier is quarantined, not on stable C

The COMMON tier (scalar / table / aggregate / cast / replacement-scan / types)
maps to DuckDB's STABLE C Extension API (`duckdb_ext_api_v1`, frozen since DuckDB
1.2.0). The ADVANCED tier (storage / index / optimizer / collation / compression /
encoding / secret / custom-FS / parser / window) has NO stable C anchor and binds
DuckDB's internal C++ ABI -- it is BLOCKED ON DUCKDB UPSTREAM shipping that C
surface. Its churn is confined to the core C++ shims and never reaches the frozen
WIT. See `docs/wit-stable-c-peg.md`.
