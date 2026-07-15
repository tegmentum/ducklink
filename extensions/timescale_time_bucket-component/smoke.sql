-- timescale_time_bucket smoke: prove LOAD + one time_bucket() dispatch through
-- the compose:dynlink/linker into the resident timescale_core-composed
-- provider. Requires the host to be started with:
--   DUCKLINK_PROVIDERS=timescale_core-composed=/path/to/timescaledb-core-provider-composed.wasm
-- (see datafission/extensions/timescaledb/deps/timescaledb-core-provider-composed.wasm).
LOAD timescale_time_bucket;
SELECT time_bucket('1 hour'::INTERVAL, TIMESTAMP '2024-01-01 12:34:56') AS bucket;
