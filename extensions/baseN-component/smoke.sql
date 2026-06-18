-- Smoke test for the `baseN` extension.
-- Run via:  tooling/smoke.py baseN   (or: make ext NAME=baseN-component)
--
-- The extension is loaded by the harness (`--load-extension baseN`); do NOT add
-- a LOAD statement here. Output runs in `.mode csv`; BLOB values render as a
-- `0x...` hex string, NULL as `NULL`.
--
-- base32: "Hello" -> JBSWY3DP (RFC 4648, no pad). Round-trips back to the bytes.
SELECT base32_encode('Hello'::BLOB) AS b32enc;
SELECT base32_decode('JBSWY3DP') AS b32dec;
-- base58: Bitcoin alphabet over a fixed 5-byte payload, then round-trip.
SELECT base58_encode('\x00\x01\x02\x03\x04'::BLOB) AS b58enc;
SELECT base58_decode('12VfUX') AS b58dec;
-- '0', 'I', 'l' are not in the base58 alphabet -> decode returns NULL, not error.
SELECT base58_decode('invalid0Il') AS b58bad;
