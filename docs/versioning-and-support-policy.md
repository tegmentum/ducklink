# ducklink Versioning & Support Policy

Status: draft. Establishes the versioning rules before the `@4.0.0` cutover so the
first real contract bump is a non-event.

## The three version axes

1. **WIT contract** — `duckdb:extension@MAJOR.MINOR.PATCH`, the host↔module
   interface. Its MAJOR is the *generation*.
2. **DuckDB version** — the database ducklink loads into. Owned by DuckDB, not us.
3. **Module version** — each wasm extension's own semver.

A module artifact is uniquely keyed by **(contract-generation, duckdb-version, platform)**.

## Core rule: lockstep major = contract generation

- The **WIT contract MAJOR is the generation**. ducklink (the host extension)
  shares it: **ducklink `4.x` speaks contract `@4`**, and modules built for it are
  generation 4.
- **User-facing rule:** host major *N* runs module generations **≤ N** — newer
  hosts are **backward-compatible** with older generations (verified across
  scalar/table/aggregate for the @2→@4 transition). A user just needs a host whose
  generation ≥ the module's.
- **Implementation (decoupled):** each module keeps its **own semver**; the catalog
  `providers[].abi` records the contract generation each provider blob was built
  for. The host resolves by selecting the **newest provider whose generation ≤ the
  host's**, falling back to the entry's top-level `content_digest`. This gives the
  simple "match on major" signal *without* overloading a module's own semver (so a
  module's independent breaking change still has room).
- **Backward-compat is not guaranteed forever.** The @2→@4 transition preserved it
  (old components still instantiate on the new host), so today's effective rule is
  "host runs generations ≤ N." A future breaking generation only requires revising
  two centralized spots — `select_provider` and the `runnable` predicate.
- **Coupling is MAJOR-only.** Additive contract changes (`@4.0 → @4.1`) are
  backward-compatible — a gen-4.0 module runs on a 4.1 host; no module rebuild is
  forced.

## The @4.0.0 cutover

- Bump **ducklink `0.5.x → 4.0.0`** to align the host version with contract
  generation 4 (semver permits a free major jump; it signals the generation).
- Reconcile the 6 straggler modules (`a5`, `jsonata`, `minijinja`, `pcap`,
  `sqlitecompat`, `tera`) into the `@4.0.0` line and rebuild them for gen-4.
- Publish a single gen-4 catalog. Retire the enriched `@2.2.0` catalog once gen-4
  is complete **and the WIT is frozen**.

## Contract-bump cadence

- MAJOR bumps are **rare, deliberate events** — only for genuinely breaking WIT
  changes. The WIT freeze policy is the lever that keeps them infrequent.
- Everything else ships as additive contract MINORs (no new-blob obligation) or as
  module minor/patch.
- Goal: **≤ 2 live generations** at any time.

## Multi-generation support window

- Support **current + previous generation (N and N-1)**.
- When N+1 ships: N-1 → `deprecated`; **EOL 6 months after deprecation** (one
  DuckDB release cycle).
- Each supported generation = a full, reproducible blob set (deterministic build
  pipeline) + catalog providers for that generation.
- Blobs are **content-addressed and immutable**, so generations coexist at ~zero
  cost. **Never delete a blob inside a supported window.**
- Surface status in the catalog (`providers[].status`: `supported` / `deprecated`
  / `eol`) and via `ducklink.capabilities` / a new `ducklink.versions` view.

## DuckDB-version axis (separate; DuckDB-owned)

- DuckDB distributes extensions per **(duckdb-version, platform)**.
  community-extensions rebuilds for **latest-stable + next** on each DuckDB
  release; **old binaries are frozen**, not updated.
- **Consequence (community channel only):** through **community-extensions** you
  cannot ship bugfixes to users on *old* DuckDB versions — its pipeline builds
  only latest-stable + next, and old binaries are frozen. This limit is specific
  to the community channel. **We can still support old versions ourselves** by
  publishing our own builds to a `custom_extension_repository` (see escape
  hatches) — we control exactly which DuckDB versions we build and patch for. The
  trade-off vs. community is ours to run (build matrix + users set the repo).
- **Escape hatches:**
  - **Self-host** via `custom_extension_repository` — you own the build matrix and
    can target any DuckDB versions.
  - **Stable C ABI (common tier only)** — one portable binary would load across
    every DuckDB with extension API ≥ `1.2.0`, carrying fixes to all versions in
    the window. **Blocked upstream today:** `libduckdb-sys` hardcodes
    `-DDUCKDB_EXTENSION_API_VERSION_UNSTABLE` (`build.rs:624`), stamping the footer
    `C_STRUCT_UNSTABLE`. The registration APIs the common tier needs
    (`register_{scalar,table,aggregate}_function`) are in the stable `v1.2.0` set,
    so portability is reachable via a duckdb-rs "stable ABI" build mode (or a
    hand-rolled entrypoint) + a symbol audit + a `C_STRUCT` / `min_duckdb_version`
    footer.

## Tier portability summary

| Tier | Surface | ABI | DuckDB-version | Platforms |
|---|---|---|---|---|
| **Common** | `ducklink_load`, `ducklink.*` views | stable C API (currently pinned by `libduckdb-sys`) | portable *if* moved to stable ABI; else per-version | all |
| **Advanced** | `LOAD WASM` | internal C++ ABI | **hard-locked** to exact DuckDB version | osx/linux only |

## Decisions & open items

- **EOL window: 6 months** after deprecation (one DuckDB cycle). ✓ decided.
- **In progress:** host provider-selection-by-generation + `providers[].status`
  + a `ducklink.versions` view (client resolves the top-level `content_digest`
  today, not a per-generation provider).
- **On hold:** the stable-ABI build-mode ask to duckdb-rs / `libduckdb-sys` (would
  let one common-tier binary span DuckDB versions). Not required for old-version
  support — self-publishing to a `custom_extension_repository` already covers
  that; revisit only as a convenience if the per-version build matrix becomes
  painful.
- **Symbol audit DONE (2026-06-30):** the common tier is stable-`v1.2.0`-clean
  EXCEPT that `duckdb-rs` executes all SQL over the UNSTABLE Arrow C-API
  (`Connection::execute` → `duckdb_execute_prepared_arrow` / `duckdb_query_arrow_schema` /
  `duckdb_arrow_rows_changed`). ducklink's own code hits it in exactly TWO spots:
  the `COMMENT ON FUNCTION` (`src/lib.rs:310`) and the `ducklink.*` view creation
  (`src/reg_duckdb.rs:1610`). All registration / vector I/O / dispatch / aggregates
  are stable; the suspected client-context / bind-result-column / vector-create
  helpers are NOT reached. So a stable-portable common tier = (1) a `libduckdb-sys`
  stable-build mode (drop the forced `-DDUCKDB_EXTENSION_API_VERSION_UNSTABLE`,
  `build.rs:624`) + (2) re-plumb those two `con.execute` calls onto stable
  `duckdb_prepare` + `duckdb_execute_prepared` + `duckdb_fetch_chunk`. Small and
  scoped, not a rewrite. Advanced tier is unaffected (inherently version-locked).
