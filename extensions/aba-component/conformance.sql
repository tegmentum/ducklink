-- Conformance suite for the `aba` logical extension (PROVIDER-NEUTRAL).
-- Run by the conformance runner against ANY certified provider unmodified; the
-- harness loads the resolved provider (do NOT add a LOAD here). Output is
-- `.mode csv` (an alias header line then the value, per SELECT).
--
-- This suite is the semantic contract for `aba` at its wit_contract: it covers
-- the surface that can drift between providers -- happy path, checksum
-- rejection, length/charset edges, and NULL propagation.

-- Happy path: real, valid US routing numbers (Chase, Wells Fargo).
SELECT aba_validate('021000021') AS chase;
SELECT aba_validate('121000248') AS wells;
-- Checksum rejection: a valid-shaped number with the last digit flipped.
SELECT aba_validate('021000020') AS bad_checksum;
-- Length edge: shorter than 9 digits, and the empty string.
SELECT aba_validate('12345') AS too_short;
SELECT aba_validate('') AS empty;
-- Charset edge: non-digit input.
SELECT aba_validate('not digits') AS junk;
-- NULL propagation: NULL in -> NULL out (the determinism/NULL contract).
SELECT aba_validate(NULL) AS null_in;
