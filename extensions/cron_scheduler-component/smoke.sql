-- cron_scheduler smoke.
-- Reference ms = 1700000000000 (2023-11-14T22:13:20Z).
-- daily-midnight '0 0 * * *' next fire from REF = 1700006400000 (2023-11-15T00:00).
-- The scheduler depends on the sibling `cron` ext for cron_next/etc.
LOAD cron;
LOAD cron_scheduler;

-- Policy scalar: skip mode collapses missed windows to the next fire after now.
SELECT cron_advance('0 0 * * *', 1699913200000, 1700000000000, 'skip') AS skip_next;
-- run_once mode fires immediately (returns now_ms) when we've missed windows.
SELECT cron_advance('0 0 * * *', 1699913200000, 1700000000000, 'run_once') AS runonce_now;
-- Normal case: last_run just fired, next fire is in the future for both policies.
SELECT cron_advance('0 0 * * *', 1700000000000 - 3600000, 1700000000000, 'skip') AS normal_next;
-- Invalid expression -> NULL.
SELECT cron_advance('garbage', 1700000000000, 1700000000000, 'skip') IS NULL AS bad_null;

-- Bootstrap SQL is text and mentions the tables + the read-only helper macro.
SELECT length(cron_bootstrap_sql()) > 0 AS has_ddl;
SELECT cron_bootstrap_sql() LIKE '%__cron_jobs%' AS mentions_jobs;
SELECT cron_bootstrap_sql() LIKE '%cron_due%' AS mentions_due;
