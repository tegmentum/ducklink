-- qrcode extension smoke (assert it produces an SVG, not the full payload).
SELECT starts_with(qr_svg('https://duckdb.org'), '<?xml') OR starts_with(qr_svg('https://duckdb.org'), '<svg') AS is_svg;
SELECT length(qr_svg('hello')) > 100 AS nontrivial;
SELECT qr_svg('x') IS NOT NULL AS short_ok;
