# DuckDB official (core) extensions on wasm

DuckDB's own first-party extensions (json, parquet, …) are **C++**, not the
Rust component extensions described in `docs/component-extension-guide.md`. The
realistic way to ship them on wasm — the same approach upstream duckdb-wasm
uses — is to **statically link them into the core**, not to wrap each as a
separate component. They then register as builtins (no runtime `LOAD` needed).

## How extension selection works

Which in-tree extensions get compiled into `artifacts/libduckdb-wasi.a` and
registered as builtins is driven by **`cmake/wasm-extension-config.cmake`**
(a list of `duckdb_extension_load(<name>)` calls), passed to DuckDB's CMake via
`-DDUCKDB_EXTENSION_CONFIGS` in `scripts/build-libduckdb-wasm.sh`.

> The `WASM_EXTENSIONS` env var is **not** the selector — it only flips DuckDB's
> internal `WASM_ENABLED` flag. The real selection is the cmake config above.

DuckDB's base config always links `core_functions` + `parquet`; everything else
is added in our config file. Each entry makes DuckDB (a) compile the extension's
C++ into the archive and (b) list it in the generated builtin-extension loader.
`crates/libduckdb-sys/build.rs` auto-discovers every `extension/<name>/lib*.a`
the build produced, so adding an extension needs no Rust-side edit.

To add one:

```bash
# 1. add a line to cmake/wasm-extension-config.cmake:
#      duckdb_extension_load(<name>)
# 2. clean rebuild the archive (WASI_TARGET_TRIPLE matters — see below):
WASI_SDK_PREFIX=… DUCKDB_SOURCE_DIR=external/duckdb ./scripts/build-libduckdb-wasm.sh
# 3. rebuild the core component and verify:
make core
./target/release/duckdb-host -- :memory: -c "SELECT …"
```

Only DuckDB's **in-tree** extensions are eligible (under
`external/duckdb/extension/`): `autocomplete`, `core_functions`, `icu`, `json`,
`parquet`, `tpch`, `tpcds`. Out-of-tree extensions (`httpfs`, `spatial`, `fts`,
`excel`, `inet`, `vss`, `sqlite_scanner`, …) live in separate repos and need
`register_external_extension` plus a wasi feasibility pass (TLS/sockets/large
deps) before they can be bundled.

## CRITICAL: build for `wasm32-wasip2`, not `wasip1-threads`

DuckDB **must** be compiled for the same wasm target the component links
against. `scripts/build-libduckdb-wasm.sh` now defaults
`WASI_TARGET_TRIPLE=wasm32-wasip2`; do not let it fall back to the toolchain
default (`wasm32-wasip1-threads`).

`wasm32-wasip1-threads` is a `-pthread` build where `errno` and `__thread`
variables are **thread-local**. In the single-threaded component runtime that
thread-local storage isn't established, so the first access faults with an
out-of-bounds memory trap. The symptom is obscure: the SQL parser traps in
`process_integer_literal` / `core_yylex` on the **first integer-literal parse**.
json is usually the first thing to hit it, because it registers SQL macros at
load (`json_group_structure` = `…->0`, an integer literal) during `open` — so it
looks like "json is broken" when the real cause is the build target. Symptoms if
you ever see them again:

- `failed to open database: … wasm trap: out of bounds memory access` with a
  backtrace through `core_yylex` / `process_integer_literal`, **or**
- queries with integer literals trap while ones without them (e.g. `json_extract`
  whose digits are inside strings) succeed.

Check `build/duckdb-wasi/compile_commands.json` for `--target=` and `-pthread`.
The toolchain **appends** flags, so a stray reconfigure without
`WASI_TARGET_TRIPLE=wasm32-wasip2` pollutes the cache — wipe `build/duckdb-wasi`
and rebuild clean if the target is wrong.

The libc++/eh archive merged in `build-libduckdb-wasm.sh` must match the same
triple (`…/lib/wasm32-wasip2/eh`), which it does automatically via
`WASI_TARGET_TRIPLE`.

## Status

| extension | status | notes |
| --- | --- | --- |
| core_functions | **working** | always linked |
| parquet | **working** | `read_parquet` + `COPY … TO … (FORMAT parquet)` verified |
| json | **working** | scalars, `::JSON`, `->>`/`->`, `json_group_array` macro, `read_json` |
| tpch | **working** | `CALL dbgen(sf=…)` generates data; `tpch_queries()` returns the 22 queries. Needs `-D_WASI_EMULATED_SIGNAL` (in the toolchain). |
| tpcds | **working** | `CALL dsdgen(sf=…)` (verified at `sf=0.01`). Needs `-D_WASI_EMULATED_SIGNAL`. **Use `sf>=0.01`** — `sf=0.001` hangs (a dsdgen tiny-scale edge case, not wasm-specific). |
| autocomplete | **working** | `sql_auto_complete('SELE')` → `SELECT` |
| icu | **working** | timezones + collations. DST-correct `AT TIME ZONE` (verified NY EDT/EST), 134 collations. Default zone from `TZ` env (`TZ=Asia/Tokyo`→`Asia/Tokyo`, else UTC), overridable with `SET TimeZone='…'`. See "ICU on wasi" below. |

## Out-of-tree official extensions

These live in **separate repos**, fetched at configure time via
`duckdb_extension_load(<name> GIT_URL <repo> GIT_TAG <commit>)`. The full
authoritative set (from DuckDB's `extension_entries.hpp`): `avro`, `aws`,
`azure`, `ducklake`, `excel`, `fts`, `httpfs`, `iceberg`, `inet`,
`mysql_scanner`, `postgres_scanner`, `spatial`, `sqlite_scanner`, `ui`, `vss`.

**Use the DuckDB-pinned commit, not `main`.** Each extension's `main` tracks the
latest DuckDB and will fail against this checkout with header-not-found errors
(API drift, e.g. fts `main` wants `duckdb/common/sql_identifier.hpp`). The
version-matched commit for *this* DuckDB is in
`external/duckdb/.github/config/extensions/<name>.cmake` (the `GIT_TAG`). Copy
that commit into `cmake/wasm-extension-config.cmake`.

wasm32-wasip2 is single-threaded with **no sockets and no TLS** (wasi-fs only),
so anything network- or large-native-dep-bound is out. Feasibility:

| extension | deps | wasi verdict |
| --- | --- | --- |
| **inet** | pure C++ (INET/IPv4/IPv6 type + funcs), no I/O | **working** — `host()`/`netmask()`/`<<=` + IPv6 (`@fe7f60b`). Caveat: bare INET-typed results render empty (use `::VARCHAR`). |
| **fts** | snowball stemmer + BM25 + SQL macros | **working** — `stem()` + `create_fts_index` + `match_bm25` (`@39376623`, `INCLUDE_DIR extension/fts/include`). |
| **vss** | pure C++ HNSW (usearch) | **working** — `CREATE INDEX … USING HNSW` + `array_distance` NN search (`@c8a4efe`, `INCLUDE_DIR src/include`). |
| **sqlite_scanner** | vendored sqlite3 + WASI VFS | **working** — `sqlite_scan(...)` + `ATTACH … (TYPE SQLITE)` read real `.sqlite` files. `-DSQLITE_OS_OTHER=1` drops sqlite3.c's unix VFS; a WASI VFS reused from `~/git/sqlite-wasm` (`cmake/sqlite-wasi-vfs/`, registered by `sqlite3_os_init`) backs file I/O. |
| **excel** | xlsx reader + vcpkg dep | **deferred** — `find_package` + `vcpkg.json`; needs a vcpkg native dep built for wasi (no vcpkg toolchain here). |
| **avro** | Apache Avro C lib + vcpkg | **deferred** — `find_path` + `vcpkg.json`; needs the Avro C lib via vcpkg built for wasi. |
| **httpfs** | HTTP/S3 over TCP+TLS | **working, out of the box** — plain `read_csv_auto('https://…')` fetches over HTTPS via `wasi:sockets` + parses (verified, iris.csv → 150 rows; secure cert verification ON, no settings). **curl is the default client on wasi** (build script patches httpfs `LoadInternal`); the vendored httplib client compiles but its connect select/poll path fails at runtime. BSD sockets come from grafting the wasip2 libc socket objects into the wasip1 core module (`scripts/build-libduckdb-wasm.sh`); openssl-wasm + curl-wasm (libcurl/nghttp2/ngtcp2/nghttp3/brotli) supply HTTP/TLS. Cert verification is secure-by-default: an embedded Mozilla CA bundle (`cmake/ca-bundle/cacert.pem`) is loaded in-memory via `CURLOPT_CAINFO_BLOB` (openssl-wasm can't load a CA *file* — its file BIO doesn't reach the wrapped host FS). |
| **ducklake** | SQL catalog + parquet storage | **working** — pure C++, no native deps. `ATTACH 'ducklake:…'` + CREATE/INSERT/SELECT verified; parquet files written + metadata persists across re-ATTACH. |
| **iceberg** | Avro manifests + roaring + (AWS/CURL skipped on wasm) | **feasible, not yet built** — upstream guards AWS SDK + CURL behind `NOT Emscripten`, so wasm needs only `avro-c` + `roaring` (both plain C/C++) built for wasi + extending the guard to `WASI`. No AWS SDK required. |
| **aws** / **azure** | cloud SDK + network | **very hard** — TCP+TLS solved by openssl-wasm, but the AWS/Azure C++ SDKs are huge; far beyond transport |
| **mysql_scanner** / **postgres_scanner** | libpq / libmysqlclient + network | **hard** — transport solved by openssl-wasm + `wasi:sockets`; still needs libpq/libmysqlclient ported to wasi |
| **spatial** | GEOS + GDAL + PROJ | **infeasible** — huge native geo stack (not a network problem; openssl-wasm doesn't help) |
| **ui** | embedded HTTP **server** | n/a — needs inbound sockets + a browser (openssl-wasm is client-side) |

**Result:** **inet, fts, vss, sqlite_scanner, httpfs, ducklake** are implemented
and verified (httpfs also covers S3 via the built-in `S3FileSystem`). `excel`/`avro` need a **vcpkg** native dep (no vcpkg-for-wasi toolchain
wired). `spatial` (geo native stack) and `ui` (inbound server + browser) stay
infeasible.

The network set is **unlocked**: httpfs fetches over HTTPS via `wasi:sockets`
(BSD sockets grafted from the wasip2 libc into the wasip1 core module; openssl-wasm
+ curl-wasm supply HTTP/TLS):

- **httpfs** — **working** (curl client). Unblocks **iceberg**/**ducklake**
  (which still need their own catalog/storage ports). Follow-ups: default to curl
  on `__wasi__`; in-memory CA verification (`CURLOPT_CAINFO_BLOB`).
- **mysql_scanner**/**postgres_scanner** — transport solved; still need
  libpq/libmysqlclient ported to wasi.
- **aws**/**azure** — transport solved, but the cloud SDKs are huge.

Other next candidates: a **vcpkg-for-wasi** toolchain (unlocks excel + avro).

## ICU on wasi

ICU assumes a generic-POSIX host and pulls in a few symbols wasi libc omits.
Four small patches make it build + run; timezone data is bundled by DuckDB, so
no external tz data is needed. Default timezone comes from the **`TZ` env var**
(the host forwards it; `TZ=America/New_York`), falling back to UTC, and
`SET TimeZone='…'` overrides per connection.

1. `cmake/toolchains/wasi-shim.hpp` — file-scope `tzname` stub (ICU reads
   `getenv("TZ")` first; this only satisfies the never-taken fallback).
2. `external/duckdb/extension/icu/.../common/putil.cpp` — `#undef U_TZSET` /
   `U_TIMEZONE` on `__wasi__` so `uprv_tzset()` is a no-op and `uprv_timezone()`
   uses the `localtime`/`gmtime` fallback (wasi lacks `tzset()` + the `timezone`
   global).
3. `.../i18n/double-conversion-utils.h` — add `__wasm__` to the
   "correct double operations" arch list (wasi-sdk doesn't define
   `__EMSCRIPTEN__`, which was the only wasm entry).
4. `crates/libduckdb-sys/build.rs` — link `wasi-emulated-mman` (ICU's common
   code uses `mmap`/`munmap`).

## `signal.h` on wasi

The tpch/tpcds data generators (`dbgen`/`dsdgen`) include `<signal.h>`, which on
wasi requires `-D_WASI_EMULATED_SIGNAL` at compile time and
`-lwasi-emulated-signal` at link time (mirrors the existing `mman` emulation).
This is wired in `cmake/toolchains/wasi-sdk.cmake` (compile + DuckDB link) and
`crates/libduckdb-sys/build.rs` (the core component's link).
