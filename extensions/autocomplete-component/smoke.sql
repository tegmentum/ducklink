-- autocomplete extension smoke: sql_complete(partial) -> table(suggestion, kind)
-- prefix-completes the LAST token of `partial` against (a) a bundled SQL keyword
-- list (kind='keyword') and -- v1.1 -- (b) live catalog TABLE names
-- (kind='table') + COLUMN names (kind='column') read through the host `query`
-- import. Prefixes below are chosen to avoid the system catalog's built-in
-- column names so the keyword cases stay deterministic across core builds.
--
-- 'WH' -> the keywords WHEN, WHERE (no built-in catalog column starts with 'wh').
SELECT suggestion, kind FROM sql_complete('WH') ORDER BY suggestion;

-- Last-token semantics: only the final token 'GR' drives the result -> 'GROUP BY'.
SELECT suggestion, kind FROM sql_complete('FROM x WHERE a GR') ORDER BY suggestion;

-- No-match prefix -> zero rows (proves the function is wired, never panics).
SELECT count(*) AS no_match FROM sql_complete('QQQ');

-- NULL argument -> zero rows.
SELECT count(*) AS null_rows FROM sql_complete(NULL);

-- v1.1 CATALOG COMPLETION. Create a table with distinctive names, then complete
-- against the live catalog. The host snapshots the catalog after this CREATE, so
-- the next sql_complete (running INSIDE its own query) sees it via the
-- re-entrancy fallback snapshot. duckdb_tables() lists only user tables, so the
-- table case is fully deterministic.
CREATE TABLE zzz_widgets(zzz_gizmo_id INTEGER, zzz_label VARCHAR);

-- 'zzz_w' prefixes the new TABLE name -> zzz_widgets (kind='table').
SELECT suggestion, kind FROM sql_complete('SELECT * FROM zzz_w') ORDER BY suggestion;

-- 'zzz_g' prefixes the new COLUMN name -> zzz_gizmo_id (kind='column').
SELECT suggestion, kind FROM sql_complete('SELECT zzz_g') ORDER BY suggestion;
