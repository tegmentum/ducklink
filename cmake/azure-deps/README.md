# azure extension on wasm — Azure SDK for C++ built for wasm32-wasip2

DuckDB's `azure` extension reads Azure Blob Storage / Data Lake (`az://`,
`abfss://`). It wraps the **Azure SDK for C++**, which DuckDB normally pulls via
vcpkg and which is why upstream excludes azure on wasm (`NOT ${WASM_ENABLED}`).
Here the SDK is **built directly for wasm32-wasip2** and merged into
libduckdb-wasi.a — the spatial/avro pattern.

## Why this works

azure-core's HTTP transport is **libcurl** (`BUILD_CURL_HTTP_TRANSPORT_ADAPTER`),
and we already have curl built for wasm (`curl-wasm`, proven by httpfs over
`wasi:sockets` with an embedded CA bundle). So the SDK's HTTP path is the same
one httpfs uses. Crypto is OpenSSL (`openssl-wasm`); the blob-list XML parser is
libxml2 (`libxml2-wasm`).

## What's built (scripts/build-azure-sdk-wasm.sh)

The 5 libraries the extension needs, compiled directly (no vcpkg/CMake) against
the wasm deps: `azure-core` (curl transport), `azure-storage-common` (+libxml2),
`azure-storage-blobs`, `azure-storage-files-datalake`, `azure-identity`. Pinned to
azure-sdk-for-cpp commit `e9f2fa3`.

## Patches / shims (this directory)

- **`azure-sdk-platform-wasi.patch`** — the SDK detects its platform from
  `__unix__`/`_WIN32`; wasm is neither, so nothing compiles. Adds `__wasi__` to
  the `AZ_PLATFORM_POSIX` branch (wasi is POSIX-like: std::fs, OpenSSL, libcurl).
- **`wasm-stubs/`** — `AzureCliCredential` shells out to the `az` CLI via
  `posix_spawn`/`pipe`/`waitpid`/`kill`, none of which exist on wasm. Stub
  `<spawn.h>` / `<sys/wait.h>` + `azure_subprocess_stubs.c` (all ENOSYS) let it
  link; that credential is unavailable at runtime, while env / connection-string
  / SAS / managed-identity credentials work.
- **`azure-5e458fcc-CMakeLists.txt.patch`** — the extension's CMakeLists, patched
  for wasm: use `build_static_extension` (a bare `add_library` leaves the
  extension out of the static link), and on wasm add the prebuilt SDK headers
  (`AZURE_SDK_WASM_DIR`, set by cmake/wasm-extension-config.cmake) instead of the
  vcpkg `find_package(...)` / `Azure::*` targets.

## Wiring

`scripts/build-libduckdb-wasm.sh`: `stage_azure_extension()` builds the SDK +
vendors/patches the extension before configure; the merge step adds the 5 Azure
libs + libxml2 into libduckdb-wasi.a (curl-wasm/openssl-wasm already merged for
httpfs). `cmake/wasm-extension-config.cmake` enables it (guarded on the built libs).

## Scope

Reads over HTTPS via the libcurl transport (same as httpfs). AzureCliCredential
unavailable; use a connection string / account key / SAS token / env credentials.
