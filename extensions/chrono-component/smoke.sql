-- chrono: only the cross-dialect datetime spellings DuckDB does NOT ship
-- as builtins. (year/month/day/date_part/date_trunc/date_diff/date_sub/
-- epoch*/make_date/make_time/make_timestamp/to_days/to_seconds/to_timestamp/
-- age/now/current_date/last_day/time_bucket are DuckDB builtins -- not
-- re-registered; date_add/datediff collide by name; extract/timestamp/
-- localtime/current_time are reserved syntax.)
-- Canonical parse / format
SELECT date_parse('2025-06-20T15:30:00Z') AS dp1;
SELECT date_parse('2025-06-20', '%Y-%m-%d') AS dp2;
SELECT date_format('2025-06-20T15:30:00Z', '%Y/%m/%d') AS df;
SELECT to_char('2025-06-20T15:30:00Z', '%Y/%m/%d') AS tc;
SELECT str_to_date('2025-06-20', '%Y-%m-%d') AS s2d;
-- tz convert
SELECT date_tz_convert('2025-06-20T12:00:00Z', 'UTC', 'America/New_York') AS tz;
-- business-day math
SELECT date_is_business_day('2025-06-21') AS bday_sat;
SELECT date_business_days_between('2024-01-01', '2024-01-08') AS bdays;
-- ISO week / year
SELECT date_iso_week('2024-01-01') AS isow;
SELECT date_iso_year('2024-12-30') AS isoy;
-- duration parse / format
SELECT duration_parse('1d 3h') AS dpar;
SELECT duration_format(97200) AS dfmt1;
SELECT duration_format(90061, 2) AS dfmt2;
-- MySQL spellings
SELECT from_unixtime(0) AS fut;
SELECT timestampdiff('day', '2024-01-01', '2024-01-08') AS tsd;
SELECT timestampadd('day', 5, '2025-06-20') AS tsa;
SELECT adddate('2025-06-20', 5) AS adddt;
SELECT subdate('2025-06-20', 5) AS subdt;
SELECT makedate(2024, 2, 29) AS mkd;
SELECT maketime(13, 45, 30) AS mkt;
-- BigQuery / Snowflake family
SELECT timestamp_seconds(0) AS tss;
SELECT timestamp_millis(1000) AS tsm;
SELECT timestamp_micros(1000000) AS tsmic;
SELECT timestamp_add('2025-06-20', 1, 'month') AS tsadd;
SELECT timestamp_sub('2025-06-20', 1, 'month') AS tssub;
SELECT timestamp_diff('2024-01-08', '2024-01-01', 'day') AS tsdiff;
SELECT timestamp_trunc('2025-06-20T15:30:45Z', 'hour') AS tst;
SELECT datetime_trunc('2025-06-20T15:30:45Z', 'day') AS dtt;
SELECT parse_date('%Y/%m/%d', '2025/06/20') AS pd;
SELECT format_date('%Y/%m/%d', '2025-06-20T15:30:00Z') AS fd;
SELECT CAST(unix_seconds('1970-01-01T00:01:00Z') AS BIGINT) AS us;
SELECT unix_millis('1970-01-01T00:00:01Z') AS um;
SELECT unix_micros('1970-01-01T00:00:01Z') AS umic;
SELECT date_from_unix_date(1) AS dfu;
SELECT date_bucket(3600, '2025-06-20T15:30:45Z') AS db;
-- non-deterministic now-aliases + version: assert non-empty
SELECT length(utc_timestamp()) > 0 AS utcts_ok;
SELECT length(sysdate()) > 0 AS sysdate_ok;
SELECT length(chrono_version()) > 0 AS ver_ok;
