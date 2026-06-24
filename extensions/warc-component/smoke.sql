-- warc extension smoke: read_warc(data BLOB) -> one row per WARC record's
-- header fields (record_no, warc_type, target_uri, content_type, content_length).
--
-- Fixture: a single valid WARC/1.0 'response' record built from chr(13)/chr(10)
-- so the CR/LF line terminators WARC requires are exact. The record body is
-- 'hello' (Content-Length: 5); the parser skips bodies, so only the header
-- fields surface. The fixture carries no Content-Type header => that column is
-- NULL, which exercises the "absent header field -> NULL" path.
SELECT record_no, warc_type, target_uri, content_type, content_length
FROM read_warc((
  'WARC/1.0' || chr(13)||chr(10) ||
  'WARC-Type: response' || chr(13)||chr(10) ||
  'WARC-Target-URI: http://example.com/' || chr(13)||chr(10) ||
  'Content-Length: 5' || chr(13)||chr(10) ||
  chr(13)||chr(10) ||
  'hello' || chr(13)||chr(10) ||
  chr(13)||chr(10)
)::BLOB)
ORDER BY record_no;

-- Malformed blob -> zero rows (proves the function is wired, never panics).
SELECT count(*) AS bad_rows FROM read_warc('not a warc'::BLOB);

-- Empty blob -> zero rows.
SELECT count(*) AS empty_rows FROM read_warc(''::BLOB);
