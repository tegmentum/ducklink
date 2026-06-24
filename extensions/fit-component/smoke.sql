-- fit extension smoke: read_fit(data BLOB) -> melted (record_no, kind, field, value).
--
-- A real .FIT activity file is a binary stream of many message kinds (file_id,
-- record, session, lap, event, ...). Usage on a real file (host-side):
--   SELECT * FROM read_fit((SELECT content FROM read_blob('activity.fit')));
-- which yields one row per data field, e.g.
--   (12,'record','heart_rate','142'), (12,'record','timestamp','2024-..'), ...
--
-- Fixture below: a hand-crafted minimal-but-valid FIT file (12-byte header +
-- one file_id definition message + one file_id data message + CRC) carrying a
-- single field: file_id.type = 'activity'. Verified against the fitparser
-- crate; the hex is the exact byte stream.
-- Melted => (1,'file_id','type','activity').
SELECT record_no, kind, field, value
FROM read_fit(unhex('0C108D080B0000002E46495440000000000100010000042F52'))
ORDER BY record_no, kind, field;

-- Malformed blob -> zero rows (proves the function is wired, never panics).
SELECT count(*) AS bad_rows FROM read_fit('not a fit file'::BLOB);

-- Empty blob -> zero rows.
SELECT count(*) AS empty_rows FROM read_fit(''::BLOB);
