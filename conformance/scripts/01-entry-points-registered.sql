-- Every SQL entry point committed in STABILITY.md § 1.1 is registered
-- and discoverable via duckdb_functions(). A host that ships this
-- surface must produce the exact rows below, in this order. The
-- schema-qualified `main.ducklink_*` names are what duckdb_functions()
-- reports when an extension registers into the default schema.

LOAD ducklink;

-- Emit `function_name | function_type` for the six committed entry
-- points. Ordered by name for a deterministic diff.
SELECT function_name, function_type
FROM duckdb_functions()
WHERE function_name IN (
    'ducklink_load',
    'ducklink_prefix',
    'PREFIX',
    'ducklink_version',
    'ducklink_help'
)
ORDER BY function_name, function_type;
