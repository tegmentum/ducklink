-- baseconv extension smoke (from_base is the inverse of DuckDB's native to_base).
SELECT from_base('ff', 16) AS from_hex;
SELECT from_base('11111111', 2) AS from_bin;
SELECT from_base('9ix', 36) AS from_b36;
SELECT from_base('z', 36) AS from_z;
SELECT from_base('zzz', 2) AS bad;
