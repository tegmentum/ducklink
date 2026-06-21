-- toml extension smoke (multiline built with chr(10): the REPL collapses
-- literal newlines inside string literals, so assemble them explicitly).
SELECT toml_to_json('title = "x"' || chr(10) || 'count = 3') AS j;
SELECT json_to_toml('{"a":1,"b":[2,3]}') AS t;
SELECT toml_to_json('not valid = = toml') AS bad;
