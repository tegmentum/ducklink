-- Conformance suite for the reference `sample_extension` (PROVIDER-NEUTRAL).
-- The harness loads the resolved provider (do NOT add a LOAD here). `.mode csv`
-- emits a header line (the AS alias) then the value, per SELECT.
SELECT sample_plus_one(41) AS plus_one;
SELECT sample_add_two(40) AS add_two;
SELECT sample_plus_one(0) AS zero;
SELECT sample_plus_one(NULL) AS null_in;
