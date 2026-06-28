-- math: only the cross-dialect scalars DuckDB does NOT ship as builtins.
-- (ceil/floor/trunc/round/abs/sign/mod/sqrt/.../pi/cot/factorial/gcd/lcm/
--  bit_count/isfinite/width_bucket/bin are DuckDB builtins -- not re-registered.)
SELECT exp2(10) AS exp2;
SELECT round(e(), 5) AS e;
SELECT rand() >= 0 AND rand() < 1 AS rand_ok;
SELECT div(17, 5) AS div;
SELECT truncate(3.99) AS trunc1;
SELECT truncate(3.14159, 2) AS trunc2;
