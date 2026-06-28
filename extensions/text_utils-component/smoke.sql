-- text-utils: only the cross-dialect string scalars DuckDB lacks as builtins.
-- (position/split_part/lcase/ucase/split/string_split/str_split/reverse are
--  DuckDB builtins; the `prefixes` table function stays DB-private.)
SELECT sql_normalize('SELECT * FROM t WHERE name=''alice'' AND age=30') AS norm;
SELECT insert('Quadratic', 3, 4, 'What') AS ins;
SELECT locate('bar', 'foobarbar') AS loc2;
SELECT locate('bar', 'foobarbar', 5) AS loc3;
