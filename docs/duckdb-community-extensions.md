# DuckDB Community Extensions on wasm — feasibility triage

The [DuckDB Community Extensions registry](https://duckdb.org/community_extensions/list_of_extensions)
(`github.com/duckdb/community-extensions`) holds **262** third-party extensions
(`h3`/`a5`, `lindel`, `crypto`, `markdown`, `bigquery`, `mongo`, …). These are
distinct from the in-repo component extensions (`isin`, `luhn`, …) and from
DuckDB's *official* extensions (`httpfs`, `spatial`, …).

They are almost all **C++**, so — like the official extensions — they can't be
runtime-loaded on wasip2 (no dynamic linking). Each must be statically compiled
against `libduckdb-wasi` with the wasi-sdk and embedded via `EMBED_EXTENSIONS` /
`ducklink compose --embed`. Feasibility is gated by three things found in each
extension's `description.yml` + repo:

1. **An `andium` ref** — the commit the community CI built against DuckDB **1.4**
   (codename *Andium* = our version). Without it there's no 1.4-compatible source.
2. **Not `wasm`-excluded** — `excluded_platforms` must not list `wasm_*` (the
   maintainer's own signal that it can't build for wasm).
3. **Native dependencies** — most C++ extensions pull libs through **vcpkg**
   (`xxhash`, `yaml-cpp`, `cmark-gfm`, `boost-math`, …). This project has **no
   vcpkg-for-wasi toolchain** (the same gap that defers `excel`), so vcpkg-dep
   extensions need their libs hand-built for wasi-sdk first.

## Triage summary (all 262)

| bucket | count | meaning |
|---|--:|---|
| **no-1.4-ref** | 191 | no Andium ref — not buildable against our DuckDB 1.4 |
| **cpp-candidate** | 37 | C++, 1.4 ref, wasm-buildable, no rust/python/vcpkg toolchain flag |
| **wasm-excluded** | 27 | maintainer excludes wasm platforms |
| **needs-rust** | 6 | wraps a Rust crate (needs a rust→wasi build, like `delta`) |
| **needs-vcpkg** | 1 | declares a vcpkg toolchain |

So **~37 are first-pass candidates**; the rest need a version bump, a vcpkg-wasi
toolchain, or a rust-wasi build.

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

## Status

| ext | tier | status |
|---|---|---|
| `read_lines` | A | **working** — `read_lines('file')` → (line_number, content, byte_offset, file_path), verified on wasi |
| `func_apply` | A | **working** — `apply` / `array_apply` / `list_apply`, verified on wasi |
| `duck_hunt` | A | **deferred** — compiles against a 1.4.x but its `andium` ref calls `GlobFiles(string, ClientContext&, …)`, drifted from our exact 1.4.0 `file_system.hpp`. Needs a one-line patch or a matched ref. |
| everything else | — | catalogued; not yet built |

First two real community extensions ported to wasip2. `duck_hunt` confirms the
triage's caveat: even Tier-A can hit per-extension 1.4.0 API drift. Both working
ones build cleanly and embed via `EMBED_EXTENSIONS="read_lines,func_apply"`.

## How to add one

```cmake
# cmake/wasm-extension-config.cmake — gated by EMBED_EXTENSIONS like the rest
embed_ext(read_lines
  GIT_URL https://github.com/teaguesterling/duckdb_read_lines
  GIT_TAG <andium-ref>
  INCLUDE_DIR src/include)
```
```bash
EMBED_EXTENSIONS="read_lines,func_apply,duck_hunt" ./scripts/build-libduckdb-wasm.sh
make core
```

## Honest assessment

Of 262, **191 have no 1.4 ref**, **27 are wasm-excluded by their maintainers**,
and most of the remaining C++ ones carry vcpkg native deps this repo can't yet
build (no vcpkg-for-wasi). The genuinely-quick set is **~10 Tier-A extensions**;
Tier B unlocks with a header-vendoring step; Tier C/network/rust are real
per-extension projects. This is long-tail work — the triage above is the map.
