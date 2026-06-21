-- pluscode extension smoke (Google HQ ~ 849VCWC8+R9).
SELECT pluscode_encode(37.4220, -122.0841, 10) AS code;
SELECT pluscode_valid('849VCWC8+R9') AS ok;
SELECT pluscode_valid('not a code') AS bad;
SELECT round(pluscode_decode_lat(pluscode_encode(40.0, -75.0, 10)), 3) AS lat;
