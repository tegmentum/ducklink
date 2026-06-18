-- Smoke test for the `isin` extension.
-- Run via:  tooling/smoke.py isin   (or: make ext NAME=isin-component)
--
-- The extension is loaded by the harness (`--load-extension isin`); do NOT add
-- a LOAD statement here. Output runs in `.mode csv`, so each SELECT emits a
-- header line (the AS alias, which doubles as a label) followed by its value.
--
-- Canonical examples from the ISO 6166 standard: Apple, Tesla, BMW -- all with
-- verified-correct check digits.
SELECT isin_validate('US0378331005') AS apple;
SELECT isin_validate('US88160R1014') AS tesla;
SELECT isin_validate('DE0005190003') AS bmw;
SELECT isin_validate('not an isin') AS junk;
SELECT isin_validate('US0378331006') AS bad_check_digit;
SELECT isin_check_digit('US0378331005') AS apple_check_digit;
SELECT isin_country('US0378331005') AS apple_country;
SELECT isin_nsin('US0378331005') AS apple_nsin;
