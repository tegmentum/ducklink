-- Smoke test for the `uuid7` extension (loaded by the harness). `.mode csv`.
-- Fully deterministic: timestamp + random hex are arguments, no clock/RNG.
SELECT uuid7_build(1700000000000, '00000000000000') AS built;
SELECT uuid7_timestamp(uuid7_build(1700000000000, '00000000000000')) AS ts;
SELECT uuid7_is_valid(uuid7_build(1700000000000, '00000000000000')) AS ok;
SELECT uuid7_is_valid('not-a-uuid') AS bad;
