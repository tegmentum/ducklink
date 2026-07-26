WASI_TARGET?=wasm32-wasip2
BROWSER_TARGET?=wasm32-unknown-unknown
# The DuckDB-wasm core lives in a separate repo (../duckdb-wasm). The core
# targets build it there and copy the artifact into this repo's target/ so
# downstream targets (precompile, host, web copy-wasm) keep finding
# ducklink_core.wasm at the usual path.
DUCKDB_WASM_DIR?=../duckdb-wasm

.PHONY: all core core-embed core-browser standalone-cli loader-stub smoke-cli smoke-cli-disk smoke-dotcmd sample-extension smoke-extension pintest-probes echo-handler smoke-httpd site site-serve ci-local clean host ext ext-smoke-all ext-list-broken ext-scaffold ext-ship iceberg-smoke tvm-test tvm-test-host precompile dotcmds cron-driver cache cache-clean sqlite-lib sqlite-loader-stub s3-wasm aws-sigv4-wasm azure-wasm gcs-wasm

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
	cargo component build -p greet-dotcmd -p core-dotcmd -p bundle-dotcmd -p prefix-dotcmd \
	  -p fieldbook-dotcmd \
	  -p duckdb-utils-schema -p duckdb-utils-data -p duckdb-utils-fts -p duckdb-utils-maint \
	  --target $(WASI_TARGET) --release
	mkdir -p artifacts/dotcmds
	cp target/$(WASI_TARGET)/release/greet_dotcmd.wasm artifacts/dotcmds/greet.wasm
	cp target/$(WASI_TARGET)/release/core_dotcmd.wasm artifacts/dotcmds/core.wasm
	cp target/$(WASI_TARGET)/release/bundle_dotcmd.wasm artifacts/dotcmds/bundle.wasm
	cp target/$(WASI_TARGET)/release/prefix_dotcmd.wasm artifacts/dotcmds/prefix.wasm
	cp target/$(WASI_TARGET)/release/fieldbook_dotcmd.wasm artifacts/dotcmds/fieldbook.wasm
	cp target/$(WASI_TARGET)/release/duckdb_utils_schema.wasm artifacts/dotcmds/duckdb-utils-schema.wasm
	cp target/$(WASI_TARGET)/release/duckdb_utils_data.wasm artifacts/dotcmds/duckdb-utils-data.wasm
	cp target/$(WASI_TARGET)/release/duckdb_utils_fts.wasm artifacts/dotcmds/duckdb-utils-fts.wasm
	cp target/$(WASI_TARGET)/release/duckdb_utils_maint.wasm artifacts/dotcmds/duckdb-utils-maint.wasm

# Build the cron-driver wasi:cli/run tool component. Unlike the dotcmds and
# extension components this is NOT a duckdb:extension — it is a standalone
# wasi:cli/run tool that drives the cron scheduler from inside wasm (imports
# duckdb:driver/exec + wasi:clocks/monotonic-clock + wasi:io/poll). The host
# loader looks for the artifact at artifacts/extensions/cron_driver_tool.wasm.
# NOTE: cargo-component emits the wasm under target/wasm32-wasip1/release/
# even when the build target is wasm32-wasip2 (it transforms the output).
cron-driver:
	cargo component build -p cron-driver-tool --target $(WASI_TARGET) --release
	mkdir -p artifacts/extensions
	cp target/wasm32-wasip1/release/cron_driver_tool.wasm artifacts/extensions/cron_driver_tool.wasm

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

# --- cache-component: wac-composed with sqlite-lib for the sqlite:extension/spi
# import + a tiny declining stub for sqlite:wasm/extension-loader.
#
# The cache-component imports sqlite:extension/spi@0.1.0 (metadata catalog on
# top of SQLite). We satisfy that at compose time rather than in the host by
# plugging sqlite-lib.wasm (from the sibling sqlite-wasm repo) into the
# component; sqlite-lib itself needs sqlite:wasm/extension-loader satisfied,
# which the tiny sqlite-loader-stub crate provides with declining impls (never
# reached — cache only calls spi.execute*). The staged
# artifacts/extensions/cache.wasm is fully self-contained wrt sqlite:*.
#
# Requires (build-only): the sibling sqlite-wasm repo checked out at
# $(SQLITE_WASM_DIR) with `scripts/setup-cargo-config.sh` already run
# (writes .cargo/config.toml from the template + wasi-sdk path).
SQLITE_WASM_DIR ?= ../sqlite-wasm
SQLITE_LIB_MODULE := $(SQLITE_WASM_DIR)/target/wasm32-wasip2/release/sqlite_lib.wasm
SQLITE_LIB_COMPONENT := $(SQLITE_WASM_DIR)/target/wasm32-wasip2/release/sqlite_lib.component.wasm

# Build sqlite-lib (SPI provider) and componentize it. Emits both the raw
# wasip2 core module and a .component.wasm we can plug with wac.
sqlite-lib:
	@ : "$${SQLITE_WASM_DIR:=../sqlite-wasm}"
	@ test -d "$(SQLITE_WASM_DIR)" \
	  || { echo "error: SQLITE_WASM_DIR=$(SQLITE_WASM_DIR) not found. Checkout tegmentum/sqlite-wasm alongside ducklink." >&2; exit 1; }
	cd $(SQLITE_WASM_DIR) && cargo build -p sqlite-lib --target wasm32-wasip2 --release
	wasm-tools component new $(SQLITE_LIB_MODULE) -o $(SQLITE_LIB_COMPONENT)

# The declining sqlite:wasm/extension-loader stub. sqlite-lib imports the
# loader through its `library` interface; this stub satisfies that import
# so the composed cache.wasm has no unresolved sqlite:* dependency.
sqlite-loader-stub:
	cargo component build -p sqlite-loader-stub --target $(WASI_TARGET) --release

# --- s3-wasm + aws-sigv4-wasm: the S3 backend the cache-component
# imports at compose time. Structurally the same story as sqlite-lib
# above -- two sibling repos build to wasm components, then wac plugs
# them together into a single self-contained artifact that satisfies
# the cache-component's `component:s3-wasm/{s3-types,s3-base,s3-aws}`
# imports. Two-stage compose:
#
#   1. aws-sigv4-wasm.wasm  --plug into->  s3-wasm.wasm
#      Result: s3_lib_self_contained.wasm exports
#      `component:s3-wasm/{s3-types,s3-base,s3-aws}` with the
#      internal `aws:sigv4/*` imports already resolved.
#
#   2. s3_lib_self_contained.wasm  --plug into->  cache.wasm
#      Result: artifacts/extensions/cache.wasm imports only WASI +
#      duckdb:extension host surface (plus wasi:http, which s3-wasm
#      routes AWS/S3 traffic over; see the wasi:http host-wiring
#      follow-up in extensions/cache-component/README.md).
#
# Sibling repos: sibling of ducklink under ~/git (or wherever the
# ducklink checkout lives). Override SIBLING_ROOT if the layout
# differs.
S3_WASM_DIR         ?= ../s3-wasm
AWS_SIGV4_WASM_DIR  ?= ../aws-sigv4-wasm
AZURE_WASM_DIR      ?= ../azure-wasm
GCS_WASM_DIR        ?= ../gcs-wasm
S3_WASM_COMPONENT        := $(S3_WASM_DIR)/target/wasm32-wasip2/release/s3_wasm.wasm
AWS_SIGV4_WASM_COMPONENT := $(AWS_SIGV4_WASM_DIR)/target/wasm32-wasip2/release/aws_sigv4_wasm.wasm
AZURE_WASM_COMPONENT     := $(AZURE_WASM_DIR)/target/wasm32-wasip2/release/azure_wasm.wasm
GCS_WASM_COMPONENT       := $(GCS_WASM_DIR)/target/wasm32-wasip2/release/gcs_wasm.wasm

# Build s3-wasm as a wasm component. `cargo build --target
# wasm32-wasip2 --release` auto-componentizes (unlike wasip1, which
# needs a follow-up `wasm-tools component new`), so no extra step.
s3-wasm:
	@ test -d "$(S3_WASM_DIR)" \
	  || { echo "error: S3_WASM_DIR=$(S3_WASM_DIR) not found. Checkout tegmentum/s3-wasm alongside ducklink." >&2; exit 1; }
	cd $(S3_WASM_DIR) && cargo build --release

# Build aws-sigv4-wasm as a wasm component (same wasip2 auto-componentize).
aws-sigv4-wasm:
	@ test -d "$(AWS_SIGV4_WASM_DIR)" \
	  || { echo "error: AWS_SIGV4_WASM_DIR=$(AWS_SIGV4_WASM_DIR) not found. Checkout tegmentum/aws-sigv4-wasm alongside ducklink." >&2; exit 1; }
	cd $(AWS_SIGV4_WASM_DIR) && cargo build --release

# Build azure-wasm as a wasm component. Same wasip2 auto-componentize
# story as s3-wasm; azure-wasm's .cargo/config.toml pins the target so
# a bare `cargo build --release` produces a componentized artifact at
# target/wasm32-wasip2/release/azure_wasm.wasm. Self-contained wrt its
# only cross-component surface: it imports wasi:{http,clocks/wall-clock,
# cli/environment}, which flow up to the final cache.wasm.
azure-wasm:
	@ test -d "$(AZURE_WASM_DIR)" \
	  || { echo "error: AZURE_WASM_DIR=$(AZURE_WASM_DIR) not found. Checkout tegmentum/azure-wasm alongside ducklink." >&2; exit 1; }
	cd $(AZURE_WASM_DIR) && cargo build --release

# Build gcs-wasm as a wasm component. Same wasip2 auto-componentize
# story as s3-wasm / azure-wasm; gcs-wasm's .cargo/config.toml pins
# the target so `cargo build --release` produces a componentized
# artifact at target/wasm32-wasip2/release/gcs_wasm.wasm. Self-
# contained wrt its only cross-component surface: it imports
# wasi:{http,clocks/wall-clock,cli/environment}, which flow up to
# the final cache.wasm. RS256 JWT signing (RustCrypto `rsa`) is
# entirely in-crate; no companion signer component.
gcs-wasm:
	@ test -d "$(GCS_WASM_DIR)" \
	  || { echo "error: GCS_WASM_DIR=$(GCS_WASM_DIR) not found. Checkout tegmentum/gcs-wasm alongside ducklink." >&2; exit 1; }
	cd $(GCS_WASM_DIR) && cargo build --release

# Full cache pipeline: build raw component + sqlite-lib + stub +
# s3-wasm + aws-sigv4-wasm + azure-wasm + gcs-wasm, then chained
# wac-plug composes and stage the fully self-contained artifact.
#
#   step 1: plug sqlite-loader-stub into sqlite-lib
#   step 2: plug aws-sigv4-wasm into s3-wasm
#   step 3: plug the sqlite composite AND the s3 composite AND
#           azure-wasm AND gcs-wasm into cache.wasm (four --plug
#           flags in a single wac invocation)
#
# azure-wasm and gcs-wasm are self-contained wrt cross-component
# surfaces (their only imports are WASI: http, clocks/wall-clock,
# cli/environment) so they go straight into the final plug without
# a preceding compose step.
cache: sqlite-lib sqlite-loader-stub s3-wasm aws-sigv4-wasm azure-wasm gcs-wasm
	cargo component build -p cache-component --target $(WASI_TARGET) --release
	mkdir -p artifacts/extensions target/compose
	wac plug $(SQLITE_LIB_COMPONENT) \
	  --plug target/wasm32-wasip1/release/sqlite_loader_stub.wasm \
	  -o target/compose/sqlite_lib_self_contained.wasm
	wac plug $(S3_WASM_COMPONENT) \
	  --plug $(AWS_SIGV4_WASM_COMPONENT) \
	  -o target/compose/s3_lib_self_contained.wasm
	wac plug target/wasm32-wasip1/release/cache.wasm \
	  --plug target/compose/sqlite_lib_self_contained.wasm \
	  --plug target/compose/s3_lib_self_contained.wasm \
	  --plug $(AZURE_WASM_COMPONENT) \
	  --plug $(GCS_WASM_COMPONENT) \
	  -o artifacts/extensions/cache.wasm
	@echo "cache: composed artifact -> artifacts/extensions/cache.wasm"

cache-clean:
	rm -f artifacts/extensions/cache.wasm \
	       target/compose/sqlite_lib_self_contained.wasm \
	       target/compose/s3_lib_self_contained.wasm \
	       $(SQLITE_LIB_COMPONENT) \
	       $(S3_WASM_COMPONENT) \
	       $(AWS_SIGV4_WASM_COMPONENT) \
	       $(AZURE_WASM_COMPONENT) \
	       $(GCS_WASM_COMPONENT)

smoke-extension:
	cargo test -p ducklink-host load_sample_extension_component

# PLAN-prefixes v1.1 THE PIN: the two probe components that make the pin flip
# VISIBLE (pintest_a -> 111, pintest_b -> 222 for the same bare pin_probe()).
# See `make smoke-dotcmd prefix` for the end-to-end flip demo.
pintest-probes:
	cargo component build -p pintest-a-component -p pintest-b-component --target $(WASI_TARGET) --release
	mkdir -p artifacts/extensions
	cp target/$(WASI_TARGET)/release/pintest_a_component.wasm artifacts/extensions/pintest_a.wasm
	cp target/$(WASI_TARGET)/release/pintest_b_component.wasm artifacts/extensions/pintest_b.wasm

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
