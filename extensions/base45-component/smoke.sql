-- Smoke test for the `base45` extension (RFC 9285).
-- Run via:  tooling/smoke.py base45   (or: make ext NAME=base45-component)
--
-- The extension is loaded by the harness (`--load-extension base45`); do NOT add
-- a LOAD statement here. Output runs in `.mode csv`. BLOB values are rendered as
-- lowercase hex via lower(hex(...)) so rows are deterministic and easy to diff.
--
-- RFC 9285 example: the 2 bytes "AB" (0x41 0x42) encode to the string "BB8".
SELECT base45_encode('AB'::BLOB) AS enc;
-- Decode "BB8" back to the original bytes; hex 4142 == "AB".
SELECT lower(hex(base45_decode('BB8'))) AS dec_hex;
-- Round-trip: encode then decode an arbitrary blob, compare hex to the input.
SELECT lower(hex(base45_decode(base45_encode('Hello!!'::BLOB)))) AS roundtrip_hex;
-- Invalid base45 (lowercase letters are outside the alphabet) -> NULL, not error.
SELECT base45_decode('invalid input') AS bad;
