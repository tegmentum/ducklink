-- roman extension smoke.
SELECT to_roman(2024) AS r;
SELECT to_roman(49) AS xlix;
SELECT from_roman('MCMLXXXIV') AS n;
SELECT from_roman('IV') AS four;
SELECT to_roman(0) AS out_of_range;
SELECT from_roman('NOPE') AS bad;
