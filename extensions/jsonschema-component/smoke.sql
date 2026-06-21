-- jsonschema extension smoke.
SELECT json_schema_valid('{"type":"integer"}', '42') AS int_ok;
SELECT json_schema_valid('{"type":"integer"}', '"hi"') AS int_bad;
SELECT json_schema_valid('{"type":"object","required":["a"]}', '{"a":1}') AS obj_ok;
SELECT json_schema_valid('{"type":"object","required":["a"]}', '{"b":2}') AS obj_missing;
SELECT json_schema_valid('not json', '1') AS bad_schema;
