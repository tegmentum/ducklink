-- httpclient extension smoke (network; http:// + https:// via pure-Rust TLS).
SELECT http_status('http://example.com/') AS http_status;
SELECT http_status('https://example.com/') AS https_status;
SELECT http_get('https://example.com/') LIKE '%Example Domain%' AS https_body;
-- http_post registration + parse-reject path (offline-safe; no flaky echo host).
SELECT http_post('not a url', 'k=v') IS NULL AS post_badurl_null;
SELECT http_status('not a url') AS bad;
