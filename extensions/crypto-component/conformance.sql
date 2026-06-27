-- Smoke test for the `crypto` extension (loaded by the harness). `.mode csv`
-- emits a header (the AS alias) then the value. Digests of the empty string and
-- "abc" are standard published test vectors.
SELECT sha1('abc') AS sha1_abc;
SELECT sha512('') AS sha512_empty;
SELECT sha3_256('abc') AS sha3_abc;
SELECT blake3('') AS blake3_empty;
SELECT crc32('123456789') AS crc32_check;
SELECT sha1(NULL) AS null_in;
