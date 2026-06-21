-- Smoke test for the `aba` extension (loaded by the harness). `.mode csv` emits
-- a header (the AS alias) then the value. 021000021 = JPMorgan Chase,
-- 121000248 = Wells Fargo (both real, valid routing numbers).
SELECT aba_validate('021000021') AS chase;
SELECT aba_validate('121000248') AS wells_fargo;
SELECT aba_validate('021000020') AS bad_check;
SELECT aba_validate('12345') AS too_short;
SELECT aba_validate('not digits') AS junk;
