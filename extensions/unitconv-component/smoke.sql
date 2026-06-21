-- unitconv extension smoke.
SELECT round(unit_convert(1, 'mi', 'km'), 6) AS mi_to_km;
SELECT round(unit_convert(100, 'C', 'F'), 1) AS boiling;
SELECT round(unit_convert(0, 'C', 'K'), 2) AS freezing_k;
SELECT round(unit_convert(1, 'lb', 'g'), 5) AS lb_to_g;
SELECT unit_convert(1, 'kg', 'm') AS cross_category;
SELECT unit_convert(1, 'foo', 'bar') AS unknown;
