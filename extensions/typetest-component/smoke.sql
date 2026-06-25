-- typetest: prove the full simple fixed-width + temporal type set round-trips.
SELECT typeof(tt_int32()) AS ti, tt_int32() AS vi;
SELECT typeof(tt_timestamp()) AS tt, tt_timestamp() AS vt;
SELECT typeof(tt_int8()) AS t, tt_int8() AS v;
SELECT typeof(tt_int16()) AS t, tt_int16() AS v;
SELECT typeof(tt_uint8()) AS t, tt_uint8() AS v;
SELECT typeof(tt_uint16()) AS t, tt_uint16() AS v;
SELECT typeof(tt_uint32()) AS t, tt_uint32() AS v;
SELECT typeof(tt_float32()) AS t, tt_float32() AS v;
SELECT typeof(tt_date()) AS t, tt_date() AS v;
SELECT typeof(tt_time()) AS t, tt_time() AS v;
-- TIMESTAMP_TZ: the bare value goes through DuckDB's duckdb_value_varchar in the
-- lean build, which returns empty for TZ; cast to VARCHAR to render the value
-- deterministically (typeof still proves the column type).
SELECT typeof(tt_timestamptz()) AS t, CAST(tt_timestamptz() AS VARCHAR) AS v;
-- DECIMAL / INTERVAL / UUID (Item-1 deferred scalar types). Render the value
-- via an explicit VARCHAR cast for deterministic output across the value API.
SELECT typeof(tt_decimal()) AS t, CAST(tt_decimal() AS VARCHAR) AS v;
SELECT typeof(tt_interval()) AS t, CAST(tt_interval() AS VARCHAR) AS v;
SELECT typeof(tt_uuid()) AS t, CAST(tt_uuid() AS VARCHAR) AS v;
