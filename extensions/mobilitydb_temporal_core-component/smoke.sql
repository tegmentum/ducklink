-- mobilitydb_temporal_core smoke: prove LOAD + registration of the ~1350
-- temporal scalars + 4 aggregates through the compose:dynlink/linker into
-- the resident mobilitydb_core-composed provider. Requires the host to be
-- started with:
--   DUCKLINK_PROVIDERS=mobilitydb_core-composed=/path/to/mobilitydb-core-provider-composed.wasm
-- (see datafission/extensions/mobilitydb/deps/mobilitydb-core-provider-composed.wasm).
-- Load-only sanity check; per-scalar behaviour is exercised by upstream
-- mobilitydb-wasm's own test-suite (this component is a pure dispatch shim).
LOAD mobilitydb_temporal_core;
SELECT 'mobilitydb_temporal_core loaded' AS status;
