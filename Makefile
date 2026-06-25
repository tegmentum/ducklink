WASI_TARGET?=wasm32-wasip2
BROWSER_TARGET?=wasm32-unknown-unknown
# The DuckDB-wasm core lives in a separate repo (../duckdb-wasm). The core
# targets build it there and copy the artifact into this repo's target/ so
# downstream targets (precompile, host, web copy-wasm) keep finding
# ducklink_core.wasm at the usual path.
DUCKDB_WASM_DIR?=../duckdb-wasm

.PHONY: all core core-embed core-browser standalone-cli loader-stub smoke-cli smoke-cli-disk smoke-dotcmd sample-extension smoke-extension echo-handler smoke-httpd site site-serve ci-local clean host ext ext-smoke-all ext-list-broken ext-scaffold ext-ship iceberg-smoke tvm-test tvm-test-host precompile dotcmds

all: core standalone-cli loader-stub dotcmds

core:
	$(DUCKDB_WASM_DIR)/scripts/sync-core-wit.sh
	@ : "$${DUCKDB_STATIC_LIB:?set DUCKDB_STATIC_LIB to the prebuilt DuckDB static archive for this target}" \
	 && : "$${DUCKDB_INCLUDE_DIR:?set DUCKDB_INCLUDE_DIR to the directory containing duckdb.h}" \
	 && cd $(DUCKDB_WASM_DIR) \
	 && cargo component build -p duckdb-component-core --target $(WASI_TARGET) --release --features wasi
	mkdir -p target/$(WASI_TARGET)/release
	cp $(DUCKDB_WASM_DIR)/target/$(WASI_TARGET)/release/ducklink_core.wasm target/$(WASI_TARGET)/release/ducklink_core.wasm

# NOTE: the embed framework (compile an extension into the core as a native
# scalar) moved ducklink-side per the duckdb-wasm split — the embeddable crates
# (isin, ...) and their embed-<name> features are NOT in ../duckdb-wasm/core.
# Re-enabling embeds needs a ducklink-side overlay over duckdb-component-core;
# until then this target is a no-op that explains the gap.
EMBED ?= embed-isin
core-embed:
	@echo "core-embed is disabled: the embed framework moved ducklink-side in the duckdb-wasm split."
	@echo "Build the plain core with 'make core'; embeddable-extension support is pending a ducklink overlay."
	@exit 1

core-browser:
	@ : "$${DUCKDB_STATIC_LIB:?set DUCKDB_STATIC_LIB to the browser-appropriate DuckDB static archive}" \
	 && : "$${DUCKDB_INCLUDE_DIR:?set DUCKDB_INCLUDE_DIR to the directory containing duckdb.h}" \
	 && cd $(DUCKDB_WASM_DIR) \
	 && cargo component build -p duckdb-component-core --target $(BROWSER_TARGET) --release --no-default-features --features browser
	mkdir -p target/$(BROWSER_TARGET)/release
	cp $(DUCKDB_WASM_DIR)/target/$(BROWSER_TARGET)/release/ducklink_core.wasm target/$(BROWSER_TARGET)/release/ducklink_core.wasm

standalone-cli:
	./scripts/sync-cli-wit.sh
	cargo component build -p ducklink-cli --target $(WASI_TARGET) --release

loader-stub:
	./scripts/sync-stub-wit.sh
	cargo component build -p ducklink-loader --target $(WASI_TARGET) --release

dotcmds:
	cargo component build -p greet-dotcmd -p core-dotcmd -p bundle-dotcmd \
	  -p duckdb-utils-schema -p duckdb-utils-data -p duckdb-utils-fts -p duckdb-utils-maint \
	  --target $(WASI_TARGET) --release
	mkdir -p artifacts/dotcmds
	cp target/$(WASI_TARGET)/release/greet_dotcmd.wasm artifacts/dotcmds/greet.wasm
	cp target/$(WASI_TARGET)/release/core_dotcmd.wasm artifacts/dotcmds/core.wasm
	cp target/$(WASI_TARGET)/release/bundle_dotcmd.wasm artifacts/dotcmds/bundle.wasm
	cp target/$(WASI_TARGET)/release/duckdb_utils_schema.wasm artifacts/dotcmds/duckdb-utils-schema.wasm
	cp target/$(WASI_TARGET)/release/duckdb_utils_data.wasm artifacts/dotcmds/duckdb-utils-data.wasm
	cp target/$(WASI_TARGET)/release/duckdb_utils_fts.wasm artifacts/dotcmds/duckdb-utils-fts.wasm
	cp target/$(WASI_TARGET)/release/duckdb_utils_maint.wasm artifacts/dotcmds/duckdb-utils-maint.wasm

smoke-cli: all
	./scripts/smoke-cli.sh

smoke-cli-disk: all
	ON_DISK_SMOKE=1 ./scripts/smoke-cli.sh

# Smoke-test the pluggable dot-command components (artifacts/dotcmds) end-to-end
# through ducklink. Needs the host + dotcmds built (covered by `all`).
smoke-dotcmd: host dotcmds
	python3 tooling/smoke-dotcmd.py

sample-extension: all
	cargo component build -p sample-extension-component --target $(WASI_TARGET) --release
	mkdir -p artifacts/extensions
	cp target/$(WASI_TARGET)/release/sample_extension_component.wasm artifacts/extensions/sample_extension.wasm

smoke-extension:
	cargo test -p ducklink-host load_sample_extension_component

# Build the reference duckdb-wasm-httpd request handler (kind='wasm' dispatch
# target). Load it with: ducklink serve --load echo=<artifact>.
echo-handler:
	cargo component build -p echo-handler --target $(WASI_TARGET) --release
	mkdir -p artifacts/handlers
	cp target/$(WASI_TARGET)/release/echo_handler.wasm artifacts/handlers/echo_handler.wasm

# duckdb-wasm-httpd end-to-end smoke (built-ins + every route kind incl. wasm).
smoke-httpd: host echo-handler
	./test/smoke-httpd.sh

# Build the extension-registry distribution database (extensions-site/registry.db)
# from registry/index.json + the built artifacts, then serve it with ducklink.
#   make site        # build registry.db
#   make site-serve  # build + serve on :8080
site:
	python3 -m pip install -q 'duckdb==1.4.0'
	python3 extensions-site/build.py

site-serve: host site
	./target/release/ducklink serve --db extensions-site/registry.db --port 8080

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
	  target/$(WASI_TARGET)/release/ducklink_core.wasm \
	  target/$(WASI_TARGET)/release/ducklink_core.cwasm
	./target/release/ducklink precompile \
	  target/$(WASI_TARGET)/release/ducklink_cli.wasm \
	  target/$(WASI_TARGET)/release/ducklink_cli.cwasm

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
	cargo build --release -p ducklink-host --bin ducklink

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
