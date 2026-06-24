-- tdigest extension smoke (build a digest over 1..100, then query it).
WITH d AS (
  SELECT tdigest(v::DOUBLE) AS td
  FROM range(1, 101) t(v)
)
SELECT round(tdigest_quantile(td, 0.5)) AS median,
       tdigest_count(td) AS cnt
FROM d;
SELECT tdigest_quantile('00'::BLOB, 0.5) AS bad_digest;
