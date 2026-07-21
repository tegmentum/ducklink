-- The ten `ducklink.*` discovery entries committed in STABILITY.md § 1.2
-- exist at the expected schema.name. A host that ships this surface must
-- produce the exact rows below, in this order. Nine are tables/views;
-- `ducklink.search` is a table macro because it takes a query argument
-- (so it appears in `duckdb_functions()`, not `information_schema.tables`)
-- — the UNION captures both categories.

LOAD ducklink;

WITH combined AS (
    SELECT table_name AS name, 'view/table' AS kind
    FROM information_schema.tables
    WHERE table_schema = 'ducklink'
    UNION ALL
    SELECT function_name AS name, 'macro' AS kind
    FROM duckdb_functions()
    WHERE schema_name = 'ducklink'
)
SELECT name, kind
FROM combined
ORDER BY name;
