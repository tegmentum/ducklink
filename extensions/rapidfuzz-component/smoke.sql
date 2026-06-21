-- Smoke test for the `rapidfuzz` extension (loaded by the harness). `.mode csv`.
SELECT round(fuzz_ratio('hello', 'hello'), 1) AS exact;
SELECT round(fuzz_ratio('hello world', 'hello wrld'), 1) AS close;
SELECT damerau_levenshtein('ca', 'ac') AS transpose;
SELECT levenshtein('ca', 'ac') AS lev_native;
SELECT indel('abc', 'axc') AS indel_sub;
SELECT osa('abcd', 'acbd') AS osa_swap;
SELECT fuzz_ratio('x', NULL) AS null_in;
