-- humantime extension smoke.
SELECT humantime_parse('1h 30m') AS secs;
SELECT humantime_parse('2 days') AS two_days;
SELECT humantime_format(5400) AS fmt;
SELECT humantime_parse('not a duration') AS bad;
