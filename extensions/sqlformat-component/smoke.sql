-- Smoke test for the `sqlformat` extension (loaded by the harness). `.mode csv`.
-- Compact form collapses whitespace onto a single line (CSV-friendly).
SELECT sql_format_compact('select  a ,b from   t') AS compact;
-- NULL in -> NULL out.
SELECT sql_format_compact(NULL) AS null_in;
-- Pretty form: 'SELECT 1' reindents to a known two-line block (2-space indent).
SELECT sql_format('SELECT 1') AS pretty;
-- Round-trip property: compacting the pretty output equals compacting the input.
SELECT sql_format_compact(sql_format('select  a ,b from   t')) = 'select a, b from t' AS roundtrip;
