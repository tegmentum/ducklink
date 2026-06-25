-- autocomplete extension smoke: sql_complete(partial) -> table(suggestion, kind)
-- prefix-completes the LAST token of `partial` against a bundled SQL keyword
-- list (kind='keyword'). Keyword + last-token-prefix completion only; catalog
-- name / context-aware completion is core-bound (see lib.rs scope note).
--
-- 'SEL' -> the single keyword 'SELECT'.
SELECT suggestion, kind FROM sql_complete('SEL') ORDER BY suggestion;

-- Last-token semantics: only the final token 'GR' drives the result -> 'GROUP BY'.
SELECT suggestion FROM sql_complete('FROM x WHERE a GR') ORDER BY suggestion;

-- A prefix that matches several keywords ('IN' -> IN, INNER JOIN, INSERT, ...).
SELECT count(*) AS in_matches FROM sql_complete('IN');

-- No-match prefix -> zero rows (proves the function is wired, never panics).
SELECT count(*) AS no_match FROM sql_complete('ZZZ');

-- NULL argument -> zero rows.
SELECT count(*) AS null_rows FROM sql_complete(NULL);
