# Package the ducklink extension-registry site into a slim runtime image.
#
# Unlike sqlite-wasm's self-contained binary, `ducklink serve` runs the wasm
# DuckDB core component via wasmtime, so the image needs THREE prebuilt
# artifacts (built on the host first — the wasm core needs the wasi-sdk +
# libduckdb pipeline, which isn't Docker-friendly):
#   1. the native `ducklink` binary, built for the IMAGE's arch (linux/amd64 for
#      Cloud Run — cross-compile or build in CI; a macOS binary won't run here)
#   2. target/wasm32-wasip2/release/ducklink_core.wasm
#   3. extensions-site/registry.db  (python3 extensions-site/build.py)
#
# `ducklink` locates the core at <compile-time workspace>/target/wasm32-wasip2/
# release/, baked in via CARGO_MANIFEST_DIR. Build the binary with the workspace
# at /src so the runtime layout below matches.
#
# Build (from repo root, after host prep):
#   make host && python3 extensions-site/build.py
#   docker build -t ducklink-extensions-site -f extensions-site/Dockerfile .
# Run:
#   docker run --rm -p 8080:8080 -e PORT=8080 ducklink-extensions-site

# ----- Stage 1: regenerate the registry DB (pinned to the core's 1.4.0 storage) -----
FROM python:3.12-slim AS db-builder
WORKDIR /src
RUN pip install --no-cache-dir 'duckdb==1.4.0'
COPY registry        /src/registry
COPY docs/assets     /src/docs/assets
COPY artifacts/extensions /src/artifacts/extensions
COPY extensions-site /src/extensions-site
RUN python3 /src/extensions-site/build.py

# ----- Stage 2: slim runtime -----
# Expects the prebuilt linux ducklink binary + core component .wasm in the build
# context (under build-artifacts/, produced by CI for linux/amd64).
FROM debian:bookworm-slim AS runtime
RUN apt-get update \
 && apt-get install --no-install-recommends -y ca-certificates \
 && rm -rf /var/lib/apt/lists/*

# The binary's compiled-in workspace_root is /src, so the core wasm must sit at
# /src/target/wasm32-wasip2/release/ and we run from /src.
WORKDIR /src
COPY build-artifacts/ducklink /usr/local/bin/ducklink
COPY build-artifacts/ducklink_core.wasm /src/target/wasm32-wasip2/release/ducklink_core.wasm
COPY --from=db-builder /src/extensions-site/registry.db /src/extensions-site/registry.db

ENV PORT=8080
EXPOSE 8080
ENTRYPOINT ["/bin/sh", "-c", "exec /usr/local/bin/ducklink serve --db /src/extensions-site/registry.db --bind 0.0.0.0 --port ${PORT}"]
