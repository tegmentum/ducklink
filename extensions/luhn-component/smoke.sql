-- Smoke test for the `luhn` extension.
-- Run via:  tooling/smoke.py luhn   (or: make ext NAME=luhn-component)
--
-- The extension is loaded by the harness (`--load-extension luhn`); do NOT add a
-- LOAD statement here. Output runs in `.mode csv`, so each SELECT emits a header
-- line (the AS alias) followed by its value.
--
-- 79927398713 is the canonical Luhn example (Wikipedia); 4111111111111111 is a
-- standard Visa test PAN. Spaces/hyphens are ignored.
SELECT luhn_validate('79927398713') AS classic;
SELECT luhn_validate('4111 1111 1111 1111') AS visa_test;
SELECT luhn_validate('79927398710') AS bad_check;
SELECT luhn_validate('hello') AS junk;
SELECT luhn_check_digit('7992739871') AS classic_check_digit;
SELECT luhn_check_digit('411111111111111') AS visa_check_digit;
