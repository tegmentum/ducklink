-- ion extension smoke: Amazon Ion <-> JSON conversion.
-- Ion text struct -> JSON text.
SELECT ion_to_json('{a:1, b:"hi"}') AS to_json;
-- JSON text -> Ion text (note Ion's `{a: 1, ...}` rendering).
SELECT ion_from_json('{"a":1,"b":"hi"}') AS from_json;
-- Round-trip JSON -> Ion -> JSON.
SELECT ion_to_json(ion_from_json('[1,2,3]')) AS roundtrip;
-- Top-level struct field as text.
SELECT ion_get('{a:1, b:2}', 'b') AS get_field;
-- NULL input -> NULL.
SELECT ion_to_json(CAST(NULL AS VARCHAR)) AS null_in;
-- Bad Ion -> NULL (never panics).
SELECT ion_to_json('{not valid') AS bad_in;
-- Bad JSON -> NULL.
SELECT ion_from_json('{not json') AS bad_json;
