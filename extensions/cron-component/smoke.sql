-- cron extension smoke. Reference ms = 1700000000000 (2023-11-14T22:13:20Z).
-- Daily midnight '0 0 * * *': next fire 2023-11-15T00:00:00Z, prev 2023-11-14T00:00:00Z.
SELECT cron_next('0 0 * * *', 1700000000000) AS nxt;
SELECT cron_prev('0 0 * * *', 1700000000000) AS prv;
SELECT cron_is_valid('* * * * *') AS ok;
SELECT cron_is_valid('bad') AS bad;
