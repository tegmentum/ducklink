-- jsonfns smoke. NOTE: json_* names COLLIDE with the json scalars currently
-- embedded in the core, so this will only LOAD + PASS against a LEAN core where
-- json has been de-embedded. Until then, correctness is proven by `cargo test`.
SELECT json_valid('{"a":1}') AS ok;
SELECT json_valid('{not json}') AS bad;
SELECT json_array_length('[1,2,3]') AS len3;
SELECT json_array_length('{"a":[1,2,3,4]}', '$.a') AS len4;
SELECT json_extract('{"a":{"b":1}}', '$.a.b') AS one;
SELECT json_extract_string('{"a":"hi"}', '$.a') AS hi;
SELECT json_type('[1,2]') AS arr;
SELECT json_type('{"a":1}', '$.a') AS bigint;
SELECT json_keys('{"a":1,"b":2}') AS keys;
SELECT json_contains('[1,2,3]', '2') AS yes;
SELECT json_contains('{"a":1,"b":2}', '{"a":1}') AS subset;
SELECT json_quote('hi') AS quoted;
