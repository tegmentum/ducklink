---
id: official-extensions
title: Official extensions on wasm
sidebar_label: Official extensions
---

# DuckDB official (core) extensions on wasm

DuckDB's own first-party extensions (`json`, `parquet`, …) are **C++**, not the
Rust component extensions. The realistic way to ship them on wasm — the same
approach upstream duckdb-wasm uses — is to **statically link them into the core**,
not to wrap each as a separate component. They then register as builtins (no
runtime `LOAD` needed). See [the lean core + de-embed
program](../architecture/lean-core.md) for how selection works via
`EMBED_EXTENSIONS`.

:::info Components vs static linking
Static-linking a C++ official extension inherits DuckDB's version-locked C++ ABI.
For the eight officials whose surface fits the existing WIT capabilities, ducklink
re-delivers the functionality as **components** (`jsonfns`, `inetfns`,
`spatialfns`, …) that register the official names directly — see
[the catalog](../catalog.md). This page tracks the static-link feasibility of the
rest.
:::

## In-tree extensions — status

These build by naming them in `EMBED_EXTENSIONS`:

| Extension | Status | Notes |
|---|---|---|
| core_functions | **working** | always linked |
| parquet | **working** | `read_parquet` + `COPY … TO … (FORMAT parquet)` |
| json | **working** | scalars, `::JSON`, `->>`/`->`, `json_group_array` macro, `read_json` |
| tpch | **working** | `CALL dbgen(sf=…)`; `tpch_queries()` returns the 22 queries. Needs `-D_WASI_EMULATED_SIGNAL`. |
| tpcds | **working** | `CALL dsdgen(sf=…)` (verified at `sf=0.01`). Use `sf >= 0.01` — `sf=0.001` hangs (a dsdgen tiny-scale edge case). |
| autocomplete | **working** | `sql_auto_complete('SELE')` → `SELECT` |
| icu | **working** | timezones + collations; DST-correct `AT TIME ZONE`, 134 collations. Default zone from `TZ` env, overridable with `SET TimeZone='…'`. |

:::warning Build for `wasm32-wasip2`
DuckDB must be compiled for the same wasm target the component links against. A
`wasm32-wasip1-threads` build traps on the first integer-literal parse (the
parser faults in `core_yylex` / `process_integer_literal`). `json` is usually the
first to hit it because it registers SQL macros at load. See [building](../guides/building.md).
:::

## Out-of-tree extensions — feasibility

These live in separate repos, fetched at configure time. `wasm32-wasip2` is
single-threaded; sockets/TLS are available via the httpfs `wasi:sockets` graft,
but threads/fork/exec are not.

:::tip Use the DuckDB-pinned commit, not `main`
Each extension's `main` tracks the latest DuckDB and fails against this checkout
with header-not-found errors (API drift). The version-matched commit is in
`external/duckdb/.github/config/extensions/<name>.cmake` (the `GIT_TAG`).
:::

| Extension | Deps | Verdict |
|---|---|---|
| **inet** | pure C++ (INET type + funcs), no I/O | **working** — `host()`/`netmask()`/`<<=` + IPv6. Caveat: bare INET-typed results render empty (use `::VARCHAR`). |
| **fts** | snowball stemmer + BM25 + SQL macros | **working** — `stem()` + `create_fts_index` + `match_bm25`. |
| **vss** | pure C++ HNSW (usearch) | **working** — `CREATE INDEX … USING HNSW` + `array_distance` NN search. |
| **sqlite_scanner** | vendored sqlite3 + WASI VFS | **working** — `sqlite_scan(...)` + `ATTACH … (TYPE SQLITE)` read real `.sqlite` files. A WASI VFS backs file I/O. |
| **encodings** | pure C++ generated charset tables | **working** — `read_csv(…, encoding='shift_jis')` and the rest of the legacy codecs decode. Large (the generated maps add ~80 MB). |
| **uc_catalog** | pure C++ + libcurl; needs httpfs + delta | **builds + loads** — `uc` secret + `uc_catalog` storage type recognized. Unverified end-to-end (needs real Databricks creds). |
| **avro** | DuckDB's avro-c fork + jansson + snappy | **working** — `read_avro('…')` with deflate + snappy + xz. Needs the **fork** `duckdb/duckdb-avro-c` + jansson + snappy + liblzma. |
| **httpfs** | HTTP/S3 over TCP+TLS | **working, out of the box** — plain `read_csv_auto('https://…')` fetches over HTTPS via `wasi:sockets` + parses (cert verification ON). curl is the default client on wasi; openssl-wasm + curl-wasm supply HTTP/TLS; an embedded Mozilla CA bundle is loaded via `CURLOPT_CAINFO_BLOB`. |
| **ducklake** | SQL catalog + parquet storage | **working** — pure C++, no native deps. `ATTACH 'ducklake:…'` + CREATE/INSERT/SELECT; metadata persists across re-ATTACH. |
| **iceberg** | avro extension + roaring; AWS SDK skipped | **working** — `iceberg_scan(...)` reads real tables, local and remote; REST catalog + bearer-token + AWS SigV4; writes work. See [the Iceberg reference](iceberg.md). |
| **excel** | xlsx reader + vcpkg dep | **deferred** — needs a vcpkg native dep built for wasi. |
| **spatial** | GEOS + PROJ + GDAL | **feasible (validated)** — GEOS/PROJ/GDAL link + run; remaining is a large build integration. |
| **aws** / **azure** | cloud SDK + network | **very hard** — transport is solved, but the AWS/Azure C++ SDKs are huge. |
| **mysql_scanner** / **postgres_scanner** | libpq / libmysqlclient + network | **hard** — transport solved; still needs the client libs ported to wasi. |
| **ui** | embedded HTTP **server** | n/a — needs inbound sockets + a browser. |

**Result:** **inet, fts, vss, sqlite_scanner, httpfs, ducklake, avro, iceberg,
encodings** are implemented and verified (httpfs also covers S3 via the built-in
`S3FileSystem`). The network set is unlocked (BSD sockets grafted from the wasip2
libc into the wasip1 core module; openssl-wasm + curl-wasm supply HTTP/TLS).

## ICU on wasi

ICU assumes a generic-POSIX host and pulls in symbols wasi libc omits. Four small
patches make it build + run; timezone data is bundled by DuckDB, so no external tz
data is needed. Default timezone comes from the `TZ` env var (falling back to
UTC), and `SET TimeZone='…'` overrides per connection. The patches stub a
file-scope `tzname`, no-op `uprv_tzset()`, add `__wasm__` to the
correct-double-operations arch list, and link `wasi-emulated-mman` (ICU uses
`mmap`/`munmap`).

## `signal.h` on wasi

The tpch/tpcds generators (`dbgen`/`dsdgen`) include `<signal.h>`, which on wasi
needs `-D_WASI_EMULATED_SIGNAL` at compile time and `-lwasi-emulated-signal` at
link time. Wired in the toolchain and `crates/libduckdb-sys/build.rs`.
