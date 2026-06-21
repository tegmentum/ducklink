-- Smoke test for the `hashfuncs` extension (loaded by the harness). `.mode csv`.
-- xxHash/Murmur values are deterministic; these are the canonical results for
-- the inputs below (seed 0).
SELECT xxh32('') AS xxh32_empty;
SELECT xxh64('') AS xxh64_empty;
SELECT xxh3('') AS xxh3_empty;
SELECT xxh64('abc') AS xxh64_abc;
SELECT murmur3('hello') AS murmur3_hello;
SELECT xxh32(NULL) AS null_in;
