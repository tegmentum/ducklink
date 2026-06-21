-- timezone extension smoke (America/New_York: winter EST -18000, summer EDT -14400).
SELECT tz_valid('America/New_York') AS ok;
SELECT tz_valid('Mars/Olympus') AS bad;
SELECT tz_offset_seconds('America/New_York', 1700000000) AS est_offset;
SELECT tz_abbreviation('America/New_York', 1700000000) AS est_abbr;
SELECT tz_offset_seconds('America/New_York', 1688000000) AS edt_offset;
SELECT tz_offset_seconds('Asia/Kolkata', 1700000000) AS india;
