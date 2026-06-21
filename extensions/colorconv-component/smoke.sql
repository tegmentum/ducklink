-- colorconv extension smoke (red #ff0000 = HSL 0,100,50 = HSV 0,100,100).
-- Use equality for hex output (a leading '#' is the expected-file comment char).
SELECT hex_to_hsl('#ff0000') AS red_hsl;
SELECT hex_to_hsv('#ff0000') AS red_hsv;
SELECT hex_to_hsl('#808080') AS gray_hsl;
SELECT hsl_to_hex(0, 100, 50) = '#ff0000' AS back_ok;
SELECT hsl_to_hex(120, 100, 50) = '#00ff00' AS green_ok;
SELECT hex_to_hsl('nothex') AS bad;
