-- hmac extension smoke (RFC 4231 / known vectors).
SELECT hmac_sha256('key', 'The quick brown fox jumps over the lazy dog') AS h256;
SELECT hmac_sha512('key', 'msg') AS h512_len;
SELECT length(hmac_sha512('key', 'msg')) AS h512_hexlen;
SELECT hmac_sha256(NULL, 'msg') AS null_key;
