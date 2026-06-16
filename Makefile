WASI_TARGET?=wasm32-wasip2
BROWSER_TARGET?=wasm32-unknown-unknown

.PHONY: all core core-browser standalone-cli smoke-cli smoke-cli-disk sample-extension smoke-extension clean

all: core standalone-cli

core:
	./scripts/sync-core-wit.sh
	@ : "$${DUCKDB_STATIC_LIB:?set DUCKDB_STATIC_LIB to the prebuilt DuckDB static archive for this target}" \
	 && : "$${DUCKDB_INCLUDE_DIR:?set DUCKDB_INCLUDE_DIR to the directory containing duckdb.h}" \
	 && cargo component build -p duckdb-core-component --target $(WASI_TARGET) --release --features wasi

core-browser:
	@ : "$${DUCKDB_STATIC_LIB:?set DUCKDB_STATIC_LIB to the browser-appropriate DuckDB static archive}" \
	 && : "$${DUCKDB_INCLUDE_DIR:?set DUCKDB_INCLUDE_DIR to the directory containing duckdb.h}" \
	 && cargo component build -p duckdb-core-component --target $(BROWSER_TARGET) --release --no-default-features --features browser

standalone-cli:
	./scripts/sync-cli-wit.sh
	cargo component build -p duckdb-cli-component --target $(WASI_TARGET) --release

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

clean:
	cargo clean
