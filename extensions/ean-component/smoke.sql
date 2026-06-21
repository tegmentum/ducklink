-- Smoke test for the `ean` extension (loaded by the harness). `.mode csv` emits
-- a header (the AS alias) then the value.
-- 4006381333931 is a standard EAN-13; 96385074 a standard EAN-8.
SELECT ean_validate('4006381333931') AS ean13_ok;
SELECT ean_validate('96385074') AS ean8_ok;
SELECT ean_validate('036000291452') AS upca_ok;
SELECT ean_validate('4006381333930') AS bad_check;
SELECT ean_check_digit('400638133393') AS ean13_check;
SELECT ean_check_digit('9638507') AS ean8_check;
