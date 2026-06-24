-- ini extension smoke. Build a tiny INI inline with chr(10) joins (the REPL
-- collapses literal newlines inside string literals, so assemble explicitly):
--   [db]
--   host=localhost
--   port=5432
SELECT ini_get('[db]' || chr(10) || 'host=localhost' || chr(10) || 'port=5432', 'db', 'host') AS host;
SELECT ini_sections('[db]' || chr(10) || 'host=localhost' || chr(10) || 'port=5432') AS sections;
SELECT ini_to_json('[db]' || chr(10) || 'host=localhost' || chr(10) || 'port=5432') AS j;
SELECT ini_get('[db]' || chr(10) || 'host=localhost', 'db', 'missing') AS absent;
