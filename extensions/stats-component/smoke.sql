-- stats: only the percentile aggregates DuckDB does NOT expose as plain
-- catalog aggregates (its percentile_cont/disc are WITHIN GROUP ordered-set
-- syntax; stddev/variance/median/mode/corr/covar/regr_*/skewness/kurtosis/
-- bit_*/any_value/array_agg/string_agg stay DuckDB builtins). p is in 0..100.
SELECT percentile(x, 50) AS p50 FROM (VALUES (1), (2), (3), (4), (5)) t(x);
SELECT percentile_cont(x, 25) AS pc25 FROM (VALUES (1), (2), (3), (4), (5)) t(x);
SELECT percentile_cont(x, 75) AS pc75 FROM (VALUES (1), (2), (3), (4), (5)) t(x);
SELECT percentile_disc(x, 50) AS pd50 FROM (VALUES (1), (2), (3), (4), (5)) t(x);
-- even-count group distinguishes cont (interpolates) from disc (actual sample)
SELECT percentile_cont(x, 50) AS pceven FROM (VALUES (1), (2), (3), (4)) t(x);
SELECT percentile_disc(x, 50) AS pdeven FROM (VALUES (1), (2), (3), (4)) t(x);
-- empty group -> NULL
SELECT percentile(x, 50) AS pempty FROM (VALUES (1)) t(x) WHERE x > 100;
