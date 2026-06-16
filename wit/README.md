# DuckDB WIT Packages

This directory is the canonical source for the WIT interfaces that describe the
DuckDB WebAssembly component stack.  Everything under `crates/*/wit/` is
generated from these definitions via the helper scripts in `scripts/`.  Please
edit the files here, then re-run `scripts/sync-core-wit.sh` and
`scripts/sync-cli-wit.sh` (and any other sync helpers) to propagate the changes
into the crate-local copies before building.

We pin the WASI Preview 2 dependencies to the latest release supported by
Wasmtime `37.0.2`, which is version `0.2.6`.  The vendored packages in
`wit/deps/` are copied directly from the upstream WASI repository at that
version so that the bindings remain stable across builds.

External DuckDB extensions can depend on the definitions in
`wit/duckdb-extension/` to stay in sync with the host runtime without having to
vendor their own copies.
