# DuckDB Community Extensions on wasm — feasibility triage

> **Status (2026-06): the feasible pure-wasm component set is built out.**
> `registry/index.json` ships **155 working component extensions** (+ the
> `sample_extension` template), `tooling/smoke.py --all` = 154 PASS + `http`
> (network-bound, exercises only with real outbound HTTP — the documented
> sandbox exception). Every Tier-A dep-free and Tier-B header-only candidate
> below has been delivered as a Rust component (reimplementing the upstream
> functionality, often via the same crates), e.g. `hashfuncs`←xxhash/murmur,
> `json_schema`←json-schema-validator, `stochastic`←boost-math (statrs),
> `celestial`, `bitfilters`, `tsid`, `crypto`. **What remains is genuinely
> blocked or deferred, not skipped:** (1) runtime primitives wasip2 lacks —
> threads/fork/exec/subprocess (`shellfs`, `system_stats`, `sshfs`); (2)
> functionality needing deep DuckDB core internals not exposed over the WIT
> boundary (`func_apply` → core function catalog; `scalarfs` → replacement-scan
> / FS internals); (3) heavy native C++ SDKs needing a per-lib wasi-sdk build
> (Tier C — effort, not a feasibility wall; deferred); (4) network-bound exts
> (feasible via the `wasi:sockets` graft but gated off by default — deferred by
> design). The sections below are the standing map for those deferred routes.


The [DuckDB Community Extensions registry](https://duckdb.org/community_extensions/list_of_extensions)
(`github.com/duckdb/community-extensions`) holds **262** third-party extensions
(`h3`/`a5`, `lindel`, `crypto`, `markdown`, `bigquery`, `mongo`, …). These are
distinct from the in-repo component extensions (`isin`, `luhn`, …) and from
DuckDB's *official* extensions (`httpfs`, `spatial`, …).

Most are **C++** (some Rust), so — like the official extensions — they can't be
runtime-loaded on wasip2 (no dynamic linking). Each must be statically compiled
against `libduckdb-wasi` (C++ via the wasi-sdk; Rust via `wasm32-wasip2`) and
embedded via `EMBED_EXTENSIONS` / `ducklink compose --embed`.

### What actually gates feasibility for us

1. **A DuckDB-1.4 (`andium`) ref** — the commit the community CI built against
   DuckDB **1.4** (codename *Andium* = our version). DuckDB's C++ extension ABI
   is version-locked and unstable *even across patch releases* (see `duck_hunt`),
   so without a 1.4-matched ref the source won't compile against our 1.4.0.
   **Only 71 of 262 have one** — this is the real ceiling without per-extension
   version-matching work.
2. **Native dependencies — effort, not a blocker.** Many C++ extensions pull
   libs through **vcpkg** (`xxhash`, `yaml-cpp`, `cmark-gfm`, `boost-math`, …).
   We don't use vcpkg, but that's not a wall: header-only deps just get vendored,
   and compiled libs get a wasi-sdk build wired in via a `<ext>-deps.cmake` that
   replaces the `find_package` — **exactly the pattern already used for the
   official extensions** (`spatial` → GEOS/PROJ/GDAL, `excel` → minizip/expat,
   `avro` → jansson/avro-c/roaring, `httpfs` → openssl/curl). It's per-dep work,
   not a feasibility gate.
3. **Rust — not a blocker.** Rust extensions build for `wasm32-wasip2`
   first-class; this repo already does it (the component extensions + the delta
   kernel). The `requires_toolchains: rust` / wasm-excluded flags are about
   DuckDB's emscripten CI, not us.
4. **Runtime primitives — the only real blocker.** Sockets are available (the
   httpfs `wasi:sockets` graft), but **threads, `fork`/`exec`/subprocess, and
   most raw syscalls are not.** Extensions that fundamentally need them
   (`shellfs` shells out, `system_stats` reads OS counters, `sshfs`) are the
   genuinely-infeasible set — and that's a small minority.

### NOT a gate: the `excluded_platforms: wasm_*` flag

A maintainer can set `excluded_platforms` to skip **DuckDB's emscripten
community-wasm CI** (`wasm_mvp`/`wasm_eh`/`wasm_threads`). That is a *different*
toolchain and runtime from our **wasip2 component model**, so it is **not** a
disqualifier here. In particular, the most common exclusion reason is "wraps a
**Rust** crate" — which the community emscripten pipeline can't build, but
which **wasm32-wasip2 builds first-class** (it's how this repo ships `delta` and
its component extensions). So Rust extensions excluded from emscripten are often
*more* feasible for us, not less (e.g. `crypto`).

## Triage summary

**71 of 262** carry a DuckDB-1.4 ref (the version-compatible set). Of those,
by the real wasip2 gate (the emscripten exclusion ignored):

| group | count | path |
|---|--:|---|
| **C++** (static-link; check vcpkg deps) | 56 | wasi-sdk static link |
| **Rust + C++** (mixed) | 9 | rust→wasip2 + C++ |
| **C++ + vcpkg** native dep | 3 | needs a wasi build of the lib |
| needs python3 build tool | 2 | — |
| **Rust** | 1 | rust→wasip2 (like `delta`) |

The other **191** have no 1.4 ref — revisitable only with per-extension
version-matching (rebase onto a 1.4-compatible commit), which DuckDB's
version-locked ABI makes a recurring chore (see below).

## The 37 candidates, by native-dependency tier

Checking each candidate's `vcpkg.json` splits them further:

**Tier A — truly dep-free (no vcpkg.json deps; pure DuckDB C++ API).** The real
easy wins:

| ext | repo | what | note |
|---|---|---|---|
| `read_lines` | teaguesterling/duckdb_read_lines | read line-based text files | clean |
| `func_apply` | teaguesterling/duckdb_func_apply | call any scalar/macro by name | clean |
| `duck_hunt` | teaguesterling/duck_hunt | parse test/CI logs (110+ formats) | clean |
| `scalarfs` | teaguesterling/duckdb_scalarfs | virtual filesystems over scalars | clean |
| `parser_tools` | hotdata-dev/duckdb_extension_parser_tools | parse referenced tables from SQL | clean |
| `celestial` | lisa-sgs/duckdb-celestial | astronomical coordinates | clean |
| `duck_block_utils` | teaguesterling/… | structured-document building | clean |
| `tsid` | quackscience/duckdb-extension-tsid | time-sortable IDs | ⚠ telemetry on load |
| `bitfilters` | query-farm/bitfilters | bloom / xor / quotient filters | ⚠ telemetry on load |
| `quickjs` | quackscience/duckdb-quickjs | embed a JS runtime | bundles QuickJS C (large) |

⚠ The query-farm / quackscience extensions ship `query_farm_telemetry.hpp`,
which phones home on load — risky/undesirable under wasi (no network by default,
may hang). Prefer the no-telemetry ones, or patch the telemetry out.

**Tier B — header-only vcpkg dep (vendorable without vcpkg).** Feasible once the
header(s) are vendored: `hashfuncs` (xxhash/rapidhash/murmurhash), `rapidfuzz`
(rapidfuzz-cpp), `json_schema` (nlohmann-json + json-schema-validator),
`stochastic` (boost-math).

**Tier C — compiled-lib vcpkg dep (needs a wasi build of the lib).** `markdown`
(cmark-gfm), `marisa` (marisa-trie), `magic` (libmagic + bzip2 + zlib), `ion`
(ion-c), `textplot` (qr generator), `yaml` (yaml-cpp), `datasketches` (Apache
DataSketches). `duckpgq` needs `openssl` — which we already have as openssl-wasm.

**Network-bound (any tier).** `http_client`, `http_request`, `cronjob`,
`web_archive`, `web_search`, `open_prompt`, `webmacro`, `netquack`, `cloudfront`
need outbound HTTP — possible via the httpfs `wasi:sockets` graft, but not pure
compute; deferred.

## The way we deliver these: as components, not static-linked C++

Static-linking a C++ community extension *works* (proven earlier with
`read_lines` + `func_apply`), but it inherits DuckDB's version-locked C++ ABI —
the very treadmill above. So the **standard is to deliver the functionality as a
Rust component** (the `duckdb:extension` WIT world) with the embed option, like
every other in-repo extension. Components have a stable WIT contract, are one
portable `.wasm`, run as a runtime `LOAD` *or* `ducklink compose --embed`, and
**don't break across DuckDB versions**. The C++ static-link route was reverted.

This catalog therefore reads as a **functionality map**: a source of useful
behaviour to reimplement as components (often with the same upstream Rust crates
the originals use), not a list of C++ binaries to chase.

| community functionality | delivered as | status |
|---|---|---|
| `crypto` (sha1/sha512/sha3-256/blake3/crc32) | `extensions/crypto-component` (Rust: sha1/sha2/sha3/blake3/crc32fast) | **working** — digests match published vectors, embeddable, version-independent |
| `read_lines`, `func_apply` | — | static-link spike, reverted (version-locked) |
| `hashfuncs`, `markdown`, `marisa`, `rapidfuzz`, … | (component candidates) | reimplement via Rust crates |
| `func_apply`-style (needs the core function catalog) | — | genuinely not a component — needs deep core access |

## How to add one (the component way)

```bash
# scaffold a component that reimplements the functionality (pulls wasm-clean crates)
python3 tooling/scaffold.py crypto --crate sha1,sha2,sha3,blake3,crc32fast
# implement the scalars/table-funcs in extensions/crypto-component/src/lib.rs, then:
make ext NAME=crypto-component        # build + smoke
ducklink compose --embed crypto       # optional: bake it into the core
```

## The deeper signal: DuckDB's C++ extension mechanism is brittle

The triage numbers are themselves evidence of a structural weakness in DuckDB's
*native* extension model, not just incidental gaps:

- **No stable ABI.** Extensions compile against a specific DuckDB version's C++
  headers and break on changes — *even across patch releases* (`duck_hunt` failed
  on a `GlobFiles(string, ClientContext&, …)` signature that drifted within 1.4.x).
- **A per-version, per-platform rebuild treadmill.** Every DuckDB release forces
  every extension to be re-pinned, rebuilt, and re-published for a matrix of
  platforms. **191 of 262 have no DuckDB-1.4 ref at all** — 1.4 has been out a
  while, so most of the community simply hasn't kept pace. That lag is the
  mechanism's maintenance burden showing through.
- **Platform-locked binaries.** Loadable `.duckdb_extension` files are
  arch/OS-specific; the coarse `excluded_platforms` flag is the only portability
  knob, and it conflates unrelated runtimes (emscripten vs wasip2).

**Why this is not a blocker for *us*.** Our product contract is the **WIT
interface + the wasm runtime**, not the DuckDB C++ API. The DuckDB version we
build against is an internal implementation detail behind that contract, so the
community's 1.4.1 lag is *their* treadmill, not ours — we pick whatever ref
builds against the core we ship. The durable path is the **component model**
(`duckdb:extension` WIT world): a stable, versioned contract, one portable
`.wasm` per extension, no recompile-per-DuckDB-version — which is exactly why the
in-repo component extensions (`isin`, `luhn`, …) don't suffer any of the above.

**Implication for this catalog.** Statically embedding a C++ community extension
inherits all of DuckDB's version-lock (we just hide it behind our WIT boundary).
It's worth doing for high-value functionality with a clean 1.4 ref, but the
strategically durable move for the long tail is to **reimplement the useful
functionality as WIT components** (as the validators already do) rather than
chase 262 version-locked C++ binaries. The triage above is the map for the
embed-the-C++ route; the component catalog is the map for the durable route.
