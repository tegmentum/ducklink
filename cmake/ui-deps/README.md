# DuckDB UI on wasm — WORKING (real SPA, offline + online)

The real DuckDB UI runs against the wasm DuckDB core. `ducklink ui` serves it
three ways:
- `--offline` (default): the genuine ui.duckdb.org SPA from captured assets
  (`web/duckdb-ui/`), no network.
- `--online`: the SPA proxied live from ui.duckdb.org.
- `--console`: a tiny built-in SQL console (fully self-contained).

In the real-UI modes `/ddb/run` returns DuckDB's exact `BinarySerializer` format,
produced by the genuine duckdb-ui C++ handlers running **inside** the wasm
component (verified: `SELECT 42 AS answer` round-trips through the bridge).

## Architecture (sqlite-wasm-httpd pattern)

httplib can't `listen()` in the wasip2 sandbox (the select/poll gap that broke
the httplib client). So the NATIVE host owns the listening socket + accept loop
(`crates/ducklink-host/src/ui_server.rs`) and bridges each request to the
component. The component runs the real `HttpServer::Handle*` logic via a chain:
host -> `handle-ui-request` (WIT export) -> `duckdb_ui_handle_request` (the C
bridge added to the ui extension) -> the private `Handle*` methods. GET asset
requests are served by the host (captured files, or proxied); `/ddb/*`, `/info`,
`/localToken`, `/localEvents` go to the component.

## Phase 1 — compile duckdb-ui for wasm

Vendored duckdb-ui @ `ded075b` (DuckDB 1.4.0) at `external/duckdb/extension/ui`.
Compiles for wasm32-wasip2 with `_WASI_EMULATED_MMAN/_SIGNAL`, `-include sys/un.h`,
`-DDUCKDB_CPP_EXTENSION_ENTRY` (selects the new loader API, no removed
`ExtensionUtil`), openssl-wasm headers, and the `wasm-stubs/net/if.h` shim
(declares `if_nametoindex`/`getnameinfo` + `NI_*`, which httplib needs but wasi's
headers don't surface in every TU).

## Phase 2 — the bridge + the rest

Patches (`ui-ded075b-*.patch`, applied by `stage_ui_extension`):
- `httplib.hpp`: guard the AF_UNIX blocks on `__wasi__` (wasi's `sockaddr_un` has
  no `sun_path`).
- `watcher.cpp`: the catalog-watcher `std::thread` becomes a no-op on wasm
  (single-threaded; also sidesteps a DuckDB catalog-API drift).
- `http_server.cpp`/`.hpp`: skip the listen thread on wasm; `Started()` reflects
  bridge mode; add `HandleRequest` (dispatches to the private `Handle*` like the
  route table) + the `duckdb_ui_handle_request`/`duckdb_ui_free` C entry.
- `ui_extension.cpp`: guard the `system()` browser-launch (the host opens the
  browser).
- `CMakeLists.txt`: `build_static_extension`; on wasm skip `find_package(OpenSSL)`
  and add the stubs/defines/openssl-wasm headers.

Host (`ui_server.rs`): pre-create `cwd/.duckdb/extension_data` (DuckDB's wasm
home is `/`, which the fs shim maps to the cwd preopen) so the open succeeds;
`open-with-config` disables extension autoinstall/autoload; `start_ui_server()`
initializes the singleton in bridge mode.

## Capturing the SPA (`scripts/capture-duckdb-ui.sh`)

The SPA is a single monolithic bundle (no lazy chunks): `index.html` + a hashed
8.3 MB `bundle.js` + css + a function-docs file. Captured to `web/duckdb-ui/`.

## Known refinement

The bridge currently forwards status + content-type + body. The handlers also set
informational `X-DuckDB-*` version response headers (the query result itself is in
the body); forwarding ALL response headers is a small follow-up (extend the bridge
wire format + the WIT record).
