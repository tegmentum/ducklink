-- msgpack extension smoke.
SELECT msgpack_to_json(msgpack_from_json('{"a":1,"b":[2,3]}')) AS roundtrip;
SELECT msgpack_from_json('[1,2,3]') AS arr_hex;
SELECT msgpack_from_json('bad json') AS bad;
