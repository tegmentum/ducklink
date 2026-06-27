-- Conformance suite for the `base45` logical extension (RFC 9285,
-- PROVIDER-NEUTRAL). Run by the conformance runner against ANY certified
-- provider unmodified; the harness loads the resolved provider (do NOT add a
-- LOAD here). Output is `.mode csv` (an alias header line then the value, per
-- SELECT). BLOBs are rendered as lowercase hex so rows are deterministic.
--
-- Covers: encode, decode, round-trip, the empty-input edge (octet_length probed
-- so the row is non-empty), invalid-decode -> NULL (not an error), and NULL
-- propagation on both functions.

-- RFC 9285 example: "AB" (0x41 0x42) -> "BB8".
SELECT base45_encode('AB'::BLOB) AS enc;
-- Decode "BB8" back to bytes; hex 4142 == "AB".
SELECT lower(hex(base45_decode('BB8'))) AS dec_hex;
-- Round-trip an arbitrary blob: hex(decode(encode(x))) == hex(x).
SELECT lower(hex(base45_decode(base45_encode('Hello!!'::BLOB)))) AS roundtrip_hex;
-- Empty-input edge: encode/decode of an empty blob round-trips to 0 bytes.
SELECT octet_length(base45_decode(base45_encode(''::BLOB))) AS empty_len;
-- Invalid base45 (chars outside the alphabet) -> SQL NULL, not an exception.
SELECT base45_decode('invalid input') AS invalid_decode;
-- NULL propagation on both functions.
SELECT base45_encode(NULL) AS null_enc;
SELECT base45_decode(NULL) AS null_dec;
