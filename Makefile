WASI_TARGET?=wasm32-wasip2
BROWSER_TARGET?=wasm32-unknown-unknown

.PHONY: all core core-embed core-browser standalone-cli loader-stub smoke-cli smoke-cli-disk sample-extension smoke-extension echo-handler smoke-httpd ci-local clean host ext ext-smoke-all ext-list-broken ext-scaffold ext-ship iceberg-smoke tvm-test tvm-test-host precompile

all: core standalone-cli loader-stub

core:
	./scripts/sync-core-wit.sh
	@ : "$${DUCKDB_STATIC_LIB:?set DUCKDB_STATIC_LIB to the prebuilt DuckDB static archive for this target}" \
	 && : "$${DUCKDB_INCLUDE_DIR:?set DUCKDB_INCLUDE_DIR to the directory containing duckdb.h}" \
	 && cargo component build -p duckdb-core-component --target $(WASI_TARGET) --release --features wasi

# Build the core with selected extensions COMPILED IN (the embed framework):
#   make core-embed EMBED=embed-isin
# Embedded extensions register as native scalars (no WIT boundary -> faster) and
# work in the standalone (wasmtime run) with no host. Add one embed-<name>
# feature per extension (see the core crate's [features]).
EMBED ?= embed-isin
core-embed:
	./scripts/sync-core-wit.sh
	@ : "$${DUCKDB_STATIC_LIB:?set DUCKDB_STATIC_LIB to the prebuilt DuckDB static archive for this target}" \
	 && : "$${DUCKDB_INCLUDE_DIR:?set DUCKDB_INCLUDE_DIR to the directory containing duckdb.h}" \
	 && cargo component build -p duckdb-core-component --target $(WASI_TARGET) --release --features wasi,$(EMBED)

core-browser:
	@ : "$${DUCKDB_STATIC_LIB:?set DUCKDB_STATIC_LIB to the browser-appropriate DuckDB static archive}" \
	 && : "$${DUCKDB_INCLUDE_DIR:?set DUCKDB_INCLUDE_DIR to the directory containing duckdb.h}" \
	 && cargo component build -p duckdb-core-component --target $(BROWSER_TARGET) --release --no-default-features --features browser

standalone-cli:
	./scripts/sync-cli-wit.sh
	cargo component build -p duckdb-cli-component --target $(WASI_TARGET) --release

loader-stub:
	./scripts/sync-stub-wit.sh
	cargo component build -p duckdb-loader-stub --target $(WASI_TARGET) --release

smoke-cli: all
	./scripts/smoke-cli.sh

smoke-cli-disk: all
	ON_DISK_SMOKE=1 ./scripts/smoke-cli.sh

sample-extension: all
	cargo component build -p sample-extension-component --target $(WASI_TARGET) --release
	mkdir -p artifacts/extensions
	cp target/$(WASI_TARGET)/release/sample_extension_component.wasm artifacts/extensions/sample_extension.wasm

smoke-extension:
	cargo test -p duckdb-component-host load_sample_extension_component

# Build the reference duckdb-wasm-httpd request handler (kind='wasm' dispatch
# target). Load it with: ducklink serve --load echo=<artifact>.
echo-handler:
	cargo component build -p echo-handler --target $(WASI_TARGET) --release
	mkdir -p artifacts/handlers
	cp target/$(WASI_TARGET)/release/echo_handler.wasm artifacts/handlers/echo_handler.wasm

# duckdb-wasm-httpd end-to-end smoke (built-ins + every route kind incl. wasm).
smoke-httpd: host echo-handler
	./test/smoke-httpd.sh

# Tiered Virtual Memory (>4 GiB spill tier) tests.
#   make tvm-test-host -- fast, pure-node free-list/handle unit test (no build)
#   make tvm-test      -- native larger-than-memory spill round-trip
#                         (needs core + cli components from `make all`)
# Opt-in >4 GiB demo (slow, ~5 GiB RAM): scripts/test-tvm-bigspill.sh
tvm-test-host:
	node web/tvm-host.test.mjs

tvm-test: host
	./scripts/test-tvm-spill.sh

# AOT-precompile the core + cli components to .cwasm so the first run skips the
# ~7s Cranelift compile (loaded via deserialize, ~0.1s). The .cwasm is CPU +
# wasmtime-version specific -- regenerate per target. Pass the .cwasm paths to
# --core-component/--cli-component to use them.
precompile: host
	./target/release/ducklink precompile \
	  target/$(WASI_TARGET)/release/duckdb_core_component.wasm \
	  target/$(WASI_TARGET)/release/duckdb_core_component.cwasm
	./target/release/ducklink precompile \
	  target/$(WASI_TARGET)/release/duckdb_cli_component.wasm \
	  target/$(WASI_TARGET)/release/duckdb_cli_component.cwasm

# Run the smoke-tests GitHub Actions workflow locally via nektos/act (Docker).
ci-local:
	./scripts/ci-local.sh

# ---- Componentized extensions (tooling/) ----------------------------------
# The extension tracking + scaffolding + smoke system mirrors ~/git/sqlite-wasm.
# Extensions load through the native host runner (ducklink); the standalone
# CLI links a no-op loader stub and cannot instantiate them. Build core + cli
# components first with `make all` (needs DUCKDB_STATIC_LIB / DUCKDB_INCLUDE_DIR).

# Native host runner that has the real component extension loader.
host:
	cargo build --release -p duckdb-component-host --bin ducklink

# Scaffold a new extension:  make ext-scaffold NAME=foo [CRATE=base32,bs58]
ext-scaffold:
	@ : "$${NAME:?set NAME to the bare extension name, e.g. NAME=isin}"
	python3 tooling/scaffold.py $(NAME) $(if $(CRATE),--crate $(CRATE),) $(if $(DESCRIPTION),--description "$(DESCRIPTION)",)

# Build one extension component, copy its artifact, and smoke it:
#   make ext NAME=isin-component
ext: host
	@ : "$${NAME:?set NAME to the extension (bare or -component), e.g. NAME=isin-component}"
	python3 tooling/smoke.py --build $(NAME)

# Smoke every extension that has a smoke.sql (assumes components already built).
ext-smoke-all: host
	python3 tooling/smoke.py --all

# Iceberg + Avro regression: generates pyiceberg fixtures and asserts the
# iceberg/avro surface (local + remote reads, codecs, time travel, REST catalog
# none/bearer/sigv4) through ducklink. Needs: pip install 'pyiceberg[snappy]' pyarrow.
iceberg-smoke: host
	python3 tooling/iceberg_smoke.py

# List upstream crates flagged in tooling/compat-registry.json.
ext-list-broken:
	python3 tooling/scaffold.py --list-broken

# Build + smoke one extension, then run the full smoke regression.
ext-ship: host
	@ : "$${NAME:?set NAME to the extension (bare or -component), e.g. NAME=isin-component}"
	python3 tooling/smoke.py --build $(NAME)
	python3 tooling/smoke.py --all

clean:
	cargo clean
