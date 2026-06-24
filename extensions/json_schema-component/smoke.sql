-- json_schema extension smoke.
-- Schema: object requiring an integer-typed "age" property.
-- Passing case: doc satisfies the schema.
SELECT json_schema_valid('{"type":"object","properties":{"age":{"type":"integer"}},"required":["age"]}', '{"age":30}') AS ok;
-- Failing case: "age" is a string, not an integer.
SELECT json_schema_valid('{"type":"object","properties":{"age":{"type":"integer"}},"required":["age"]}', '{"age":"old"}') AS bad;
-- Errors array is empty for a valid doc.
SELECT json_schema_errors('{"type":"integer"}', '5') AS no_errs;
-- Errors array is non-empty for an invalid doc.
SELECT json_array_length(json_schema_errors('{"type":"integer"}', '"hello"')) AS n_errs;
-- NULL input yields NULL output.
SELECT json_schema_valid(NULL, '{"age":30}') AS nul;
