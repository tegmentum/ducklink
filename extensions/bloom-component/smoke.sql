-- bloom extension smoke (build a filter over fruits, then test membership).
WITH f AS (SELECT bloom_filter(v) AS bf FROM (VALUES ('apple'),('banana'),('cherry')) t(v))
SELECT bloom_contains(bf, 'apple') AS has_apple,
       bloom_contains(bf, 'banana') AS has_banana,
       bloom_contains(bf, 'durian') AS has_durian
FROM f;
SELECT bloom_contains('00', 'x') AS bad_filter;
