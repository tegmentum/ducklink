# Full DuckDB UI on wasm — port status & roadmap

Two things ship for "the DuckDB UI on wasm":

1. **A working equivalent console (DONE).** `duckdb-host ui` serves a small
   offline SQL console that bridges queries to the core component
   (`crates/duckdb-component-host/src/ui_server.rs`, `test/smoke-ui.sh`). This is
   live and committed.

2. **The *real* DuckDB UI, offline (IN PROGRESS).** Capturing the actual
   ui.duckdb.org SPA + running the genuine duckdb-ui handlers so the SPA's exact
   protocol works. This directory tracks that port.

## Why a port (not a host reimplementation)

The SPA's `/ddb/run` decodes DuckDB's internal `BinarySerializer`(ArrowSchema/
ArrowArray) — not standard Arrow IPC — so the real handlers must run inside the
component. And httplib's `listen()` can't run in the wasip2 sandbox (the same
select/poll gap that broke the httplib client). So: the NATIVE host owns the
listening socket (sqlite-wasm-httpd pattern) and bridges each request to the
component, which runs the real `HttpServer::Handle*` logic.

## Phase 1 — compile duckdb-ui for wasm: DONE

Vendored duckdb-ui @ `ded075b` (the DuckDB 1.4.0 build) at
`external/duckdb/extension/ui`. All sources compile for wasm32-wasip2 with:
- defines: `_WASI_EMULATED_MMAN _WASI_EMULATED_SIGNAL -include sys/un.h
  -DUI_EXTENSION_SEQ_NUM=... -DUI_EXTENSION_GIT_SHA=... -DDUCKDB_CPP_EXTENSION_ENTRY
  -DCPPHTTPLIB_OPENSSL_SUPPORT` (+ openssl-wasm includes). The
  `DUCKDB_CPP_EXTENSION_ENTRY` define makes `RegisterTF` use the new
  `loader.RegisterFunction` API, sidestepping the removed `ExtensionUtil`.
- `wasm-stubs/net/if.h` — httplib references `<net/if.h>` (never called; listen
  is bypassed).
- `ui-ded075b-httplib.hpp.patch` — guard httplib's two AF_UNIX blocks with
  `!defined(__wasi__)` (wasi's `sockaddr_un` is a stub without `sun_path`).
- `ui-ded075b-watcher.cpp.patch` — the catalog watcher is a background
  `std::thread` (live schema refresh); wasm is single-threaded, so `Start/Stop/
  Watch` become no-ops. Also sidesteps a DuckDB API drift in its catalog walk.

## Phase 2-7 — remaining

2. **Bridge** — add a public `HttpServer::HandleRequest(method, path, headers,
   body) -> {status, ctype, body}` that dispatches to the private `Handle*`
   (constructing `httplib::Request`/`Response` + a one-shot `ContentReader` for
   run/tokenize); make `start_ui()` initialize the singleton WITHOUT the
   listen/thread on wasm; export an `extern "C"` symbol. `/localEvents` (SSE)
   returns empty (the watcher is disabled).
3. **WIT + core component** — a `handle-ui-request` export that FFI-calls the
   bridge symbol.
4. **Host server** — `duckdb-host ui` forwards every request to the export.
5. **Build** — wire the extension into libduckdb (CMakeLists patch for the stubs/
   defines/openssl link + `build_static_extension`), rebuild libduckdb + core.
6. **Assets + online/offline toggle** — capture the SPA (index + ~8.3 MB bundle +
   css + svgs + ~300 lazy chunks), embed for offline; a `--offline/--online` flag
   chooses embedded assets vs proxying ui.duckdb.org (the default upstream behavior).
7. **Browser test.**
