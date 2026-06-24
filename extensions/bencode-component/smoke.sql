-- bencode extension smoke (BitTorrent bencoding).
-- dict {bar: "spam", foo: 42}  (keys sorted: bar < foo)
SELECT bencode_to_json('d3:bar4:spam3:fooi42ee'::BLOB) AS obj;
-- list ["spam", "eggs"]
SELECT bencode_to_json('l4:spam4:eggse'::BLOB) AS arr;
-- integer 42
SELECT bencode_to_json('i42e'::BLOB) AS num;
-- byte-string "spam"
SELECT bencode_to_json('4:spam'::BLOB) AS str;
-- validity checks
SELECT bencode_is_valid('i42e'::BLOB) AS ok;
SELECT bencode_is_valid('d3:bar4:spam3:fooi42ee'::BLOB) AS ok_dict;
SELECT bencode_is_valid('i42'::BLOB) AS bad_unterminated;
SELECT bencode_is_valid('xyz'::BLOB) AS bad_garbage;
-- decode error -> NULL
SELECT bencode_to_json('not-bencode'::BLOB) AS bad;
