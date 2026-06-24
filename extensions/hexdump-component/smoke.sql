-- Smoke test for the `hexdump` extension (loaded by the harness). `.mode csv`.
SELECT hex_pretty(unhex('deadbeef')) AS deadbeef;
SELECT hexdump(unhex('48656c6c6f')) AS hello;
SELECT hex_pretty(NULL) AS null_in;
