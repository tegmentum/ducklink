-- deltascan smoke: inspect a Delta Lake _delta_log (concatenated JSON-lines).
-- delta_log_info(log_json) -> (version, action, path, size) one row per action;
-- delta_schema(log_json)   -> (column_name, column_type) from the metaData schema.
--
-- Fixture: a minimal 3-line _delta_log -- a protocol action, a metaData action
-- whose schemaString describes columns a(integer) and b(string), and one add
-- action for part-0001.parquet (size 1234). Lines are joined with chr(10) so the
-- log is genuine JSON-lines. The inner schemaString is a JSON-encoded string, so
-- its quotes are backslash-escaped exactly as Delta writes them.
--
-- One reusable log literal, built once per query below.

-- delta_log_info emits all three actions; the add carries path + size.
SELECT version, action, path, size
FROM delta_log_info(
  '{"protocol":{"minReaderVersion":1,"minWriterVersion":2}}' || chr(10) ||
  '{"metaData":{"id":"t","schemaString":"{\"type\":\"struct\",\"fields\":[{\"name\":\"a\",\"type\":\"integer\",\"nullable\":true,\"metadata\":{}},{\"name\":\"b\",\"type\":\"string\",\"nullable\":true,\"metadata\":{}}]}","partitionColumns":[]}}' || chr(10) ||
  '{"add":{"path":"part-0001.parquet","size":1234,"dataChange":true}}'
)
ORDER BY action;

-- delta_schema parses the metaData schemaString into (a:integer, b:string).
SELECT column_name, column_type
FROM delta_schema(
  '{"protocol":{"minReaderVersion":1,"minWriterVersion":2}}' || chr(10) ||
  '{"metaData":{"id":"t","schemaString":"{\"type\":\"struct\",\"fields\":[{\"name\":\"a\",\"type\":\"integer\",\"nullable\":true,\"metadata\":{}},{\"name\":\"b\",\"type\":\"string\",\"nullable\":true,\"metadata\":{}}]}","partitionColumns":[]}}' || chr(10) ||
  '{"add":{"path":"part-0001.parquet","size":1234,"dataChange":true}}'
)
ORDER BY column_name;

-- Garbage log -> zero rows (proves the function is wired, never panics).
SELECT count(*) AS bad_rows FROM delta_log_info('not a delta log');

-- Empty log -> zero schema rows.
SELECT count(*) AS empty_schema FROM delta_schema('');
