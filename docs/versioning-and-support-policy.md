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
- **User-facing rule:** host major *N* loads module generation *N*.
- **Implementation (decoupled):** each module keeps its **own semver**; the catalog
  `providers[].abi` records the contract generation each provider blob was built
  for. The host resolves by selecting the provider whose contract major == the
  host's. This gives the simple "match on major" signal *without* overloading a
  module's own semver (so a module's independent breaking change still has room).
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
- When N+1 ships: N-1 → `deprecated`; **EOL after one release cycle** (or a fixed
  window — TBD, proposed 6 months).
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
- **Consequence:** you **cannot** ship bugfixes to users on *old* DuckDB versions
  through community-extensions — new work only flows forward. Users on an old
  DuckDB keep the frozen binary until they upgrade DuckDB.
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

## Open items

- Implement **host provider-selection-by-contract-major** (client resolves the
  top-level `content_digest` today, not a per-generation provider).
- Add `providers[].status` + a `ducklink.versions` view.
- Decide the **EOL window** duration (proposed: 6 months / one DuckDB cycle).
- Upstream: propose a **stable-ABI build mode** to duckdb-rs / `libduckdb-sys`;
  audit that the common tier links nothing outside the stable `v1.2.0` set.
