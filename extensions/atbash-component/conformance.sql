-- Conformance suite for the `atbash` logical extension (PROVIDER-NEUTRAL).
-- Run by the conformance runner against ANY certified provider unmodified; the
-- harness loads the resolved provider (do NOT add a LOAD here). Output is
-- `.mode csv` (an alias header line then the value, per SELECT).
--
-- Atbash is a self-inverse substitution cipher (a<->z within each case;
-- non-letters pass through). Covers: basic mapping, mixed-case + punctuation
-- passthrough, the self-inverse round-trip, the empty-input edge (length probed
-- so the row is non-empty), and NULL propagation.

-- Basic mapping: 'abc' -> 'zyx'.
SELECT atbash('abc') AS basic;
-- Mixed case + punctuation passthrough (csv quotes the comma).
SELECT atbash('Hello, World!') AS mixed;
-- Self-inverse: atbash(atbash(x)) == x.
SELECT atbash(atbash('roundtrip')) AS selfinverse;
-- Empty-input edge: maps to the empty string (length 0), not NULL.
SELECT length(atbash('')) AS empty_len;
-- NULL propagation: NULL in -> NULL out.
SELECT atbash(NULL) AS null_in;
