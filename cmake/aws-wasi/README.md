# aws extension on wasm — WORKING (native credential resolution)

DuckDB's `aws` extension works on wasm32-wasip2: `load_aws_credentials()` and
`CREATE SECRET (TYPE s3, PROVIDER credential_chain)` resolve AWS credentials and
hand them to httpfs for S3 access (verified — see `test/smoke-aws.sh`).

The extension normally wraps the **AWS C++ SDK** (aws-cpp-sdk-core/sts/sso/
identity-management), which does not build for wasm — which is why DuckDB
upstream excludes it (`NOT ${WASM_ENABLED}`). But the extension only uses the SDK
to resolve credentials + region from the standard sources, and the
**non-network** ones are trivial to read directly. So on wasm the SDK is replaced
by a small native resolver — no AWS SDK, no extra deps.

## What works

- **env** provider — `AWS_ACCESS_KEY_ID` / `AWS_SECRET_ACCESS_KEY` /
  `AWS_SESSION_TOKEN` (the host runner inherits the environment).
- **config** provider — the INI credentials file (`~/.aws/credentials` or
  `AWS_SHARED_CREDENTIALS_FILE`) and config file (`~/.aws/config` or
  `AWS_CONFIG_FILE`), including named profiles (`[name]` / `[profile name]`).
- **region** — `AWS_REGION` / `AWS_DEFAULT_REGION`, else the profile's `region`.
- the default chain (`env;config`) and an explicit `CHAIN '...'`.

## What doesn't (errors clearly)

`sso`, `sts` (assume-role), `instance` (EC2 metadata) and `process` need HTTP or
a child process; they throw a `NotImplementedException` pointing the user at the
`env`/`config` providers. (A future version could route sso/sts/instance through
httpfs's HTTP stack.)

## Files

- **`aws_wasi_credentials.hpp`** — the native resolver (INI parser + env reader +
  chain/region logic), header-only, used under `#ifdef __wasi__`. Dropped into
  the vendored extension's `src/include/` by the build script.
- **`aws-812ce80-*.patch`** — patches against duckdb-aws @ `812ce80` (the commit
  DuckDB's own `.github/config/extensions/aws.cmake` pins for this DuckDB):
  - `aws_secret.cpp` / `aws_extension.cpp`: `#ifdef __wasi__` branches that call
    the native resolver instead of the SDK; guard the SDK includes + the
    STS/curl-cert bits.
  - `CMakeLists.txt`: skip `find_package(AWSSDK)` + the SDK linking on wasm, and
    use `build_static_extension` (the bare `add_library` left the extension out
    of the static link, so the generated loader's `AwsExtension::Load` was
    undefined).

## Wiring

`scripts/build-libduckdb-wasm.sh` (`stage_aws_extension`) vendors duckdb-aws @
the pin, applies these patches + the header (idempotent), before configure;
`cmake/wasm-extension-config.cmake` does `duckdb_extension_load(aws SOURCE_DIR …)`.
Pure C++ — it links into libduckdb-wasi.a with no extra archives.
