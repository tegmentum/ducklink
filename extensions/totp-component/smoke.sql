-- totp extension smoke (RFC 6238 SHA-1 test vector: secret=base32('12345...890'),
-- T=59s, period=30, 8 digits -> 94287082).
SELECT totp('GEZDGNBVGY3TQOJQGEZDGNBVGY3TQOJQ', 59, 30, 8) AS code_t59;
SELECT totp('GEZDGNBVGY3TQOJQGEZDGNBVGY3TQOJQ', 1111111109, 30, 8) AS code_2005;
SELECT totp('!!notbase32!!', 59, 30, 6) AS bad;
