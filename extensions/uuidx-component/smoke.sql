-- Smoke test for the `uuidx` extension (loaded by the harness). `.mode csv`.
-- uuid_v7() is non-deterministic; assert its shape, not its value.
SELECT length(uuid_v7()) AS v7_len;
SELECT uuid_version(uuid_v7()) AS v7_version;
SELECT uuid_version('00000000-0000-4000-8000-000000000000') AS v4_version;
SELECT uuid_version('not-a-uuid') AS bad;
SELECT uuid_timestamp('017f22e2-79b0-7cc3-98c4-dc0c0c07398f') AS v7_ts;
SELECT uuid_timestamp('00000000-0000-4000-8000-000000000000') AS v4_ts_null;
