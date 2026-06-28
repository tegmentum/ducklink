-- talib smoke: TA-Lib indicators as DuckDB window functions over a frame.
-- The indicators register as aggregates, so DuckDB's window machinery
-- drives them over an `OVER (... ROWS BETWEEN ...)` frame (3-period here).
-- Whole-table aggregate forms (no OVER) double as plain-aggregate checks.
SELECT round(sma(c), 4) AS sma_all FROM (SELECT * FROM (VALUES (10.0),(11.0),(12.0)) AS t(c));
SELECT round(ema(c), 4) AS ema_all FROM (SELECT * FROM (VALUES (1.0),(2.0),(3.0)) AS t(c));
SELECT round(rsi(c), 4) AS rsi_all FROM (SELECT * FROM (VALUES (1.0),(2.0),(3.0),(4.0)) AS t(c));
-- 3-period windowed SMA over t=1..5 of closes 10,11,12,13,14.
SELECT t, round(sma(c) OVER w, 4) AS sma3
FROM (VALUES (1,10.0),(2,11.0),(3,12.0),(4,13.0),(5,14.0)) AS t(t,c)
WINDOW w AS (ORDER BY t ROWS BETWEEN 2 PRECEDING AND CURRENT ROW)
ORDER BY t;
