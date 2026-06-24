-- bitfilters extension smoke (build an xor filter over a small int set, then
-- test membership).
WITH f AS (SELECT xor_filter(v) AS xf FROM (VALUES (10),(20),(30),(40),(50)) t(v))
SELECT xor_filter_contains(xf, 10) AS has_10,
       xor_filter_contains(xf, 30) AS has_30,
       xor_filter_contains(xf, 50) AS has_50,
       xor_filter_contains(xf, 999999) AS has_999999
FROM f;
SELECT xor_filter_contains('00'::BLOB, 10) AS bad_filter;
