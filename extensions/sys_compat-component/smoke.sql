-- sys_compat: only the DB-identity scalars DuckDB does NOT ship as builtins
-- (version/current_user/session_user/user/current_role/current_database/
-- current_schema/current_schemas/format_bytes are DuckDB builtins). These
-- return DuckDB-appropriate constants.
SELECT system_user() AS su;
SELECT database() AS db;
SELECT schema() AS sch;
SELECT collation('x') AS col;
