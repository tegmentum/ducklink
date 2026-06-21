-- cbor extension smoke (JSON keys sorted by serde_json's BTreeMap).
SELECT cbor_to_json(cbor_from_json('{"a":1,"b":[2,3]}')) AS roundtrip;
SELECT cbor_from_json('[1,2,3]') AS arr_hex;
SELECT cbor_to_json('83010203') AS arr_back;
SELECT cbor_from_json('not json') AS bad;
