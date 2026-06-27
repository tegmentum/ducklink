-- Conformance suite for the `ascii85` logical extension (PROVIDER-NEUTRAL).
-- Run by the conformance runner against ANY certified provider unmodified; the
-- harness loads the resolved provider (do NOT add a LOAD here). Output is
-- `.mode csv` (an alias header line then the value, per SELECT).
--
-- Covers the semantic surface that can drift: encode happy path, decode
-- round-trip, empty input, invalid-decode -> NULL (not an error), and NULL
-- propagation on both functions.

-- Encode happy path: "Hello" -> <~87cURDZ~>.
SELECT ascii85_encode('Hello') AS enc;
-- Round-trip: decode(encode(x)) == x.
SELECT ascii85_decode(ascii85_encode('round trip!')) AS roundtrip;
-- Empty input edge: encodes to the empty frame, not NULL.
SELECT ascii85_encode('') AS empty_enc;
-- Invalid decode -> SQL NULL (the error-condition contract), not an exception.
SELECT ascii85_decode('!!!bad!!!') AS invalid_decode;
-- NULL propagation on both functions.
SELECT ascii85_encode(NULL) AS null_enc;
SELECT ascii85_decode(NULL) AS null_dec;
