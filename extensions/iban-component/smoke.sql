-- Smoke test for the `iban` extension. Loaded by the harness (--load-extension
-- iban); `.mode csv` emits a header line (the AS alias) then the value.
-- GB82WEST... and DE89... are the canonical ISO 13616 examples.
SELECT iban_validate('GB82 WEST 1234 5698 7654 32') AS gb_valid;
SELECT iban_validate('DE89370400440532013000') AS de_valid;
SELECT iban_validate('GB82 WEST 1234 5698 7654 33') AS bad_check;
SELECT iban_validate('not an iban') AS junk;
SELECT iban_country('GB82WEST12345698765432') AS gb_country;
SELECT iban_bban('GB82WEST12345698765432') AS gb_bban;
