-- http extension smoke (network; http:// + https:// via pure-Rust TLS).
SELECT http_status('http://example.com/') AS http_status;
SELECT http_status('https://example.com/') AS https_status;
SELECT http_get('https://example.com/') LIKE '%Example Domain%' AS https_body;
SELECT http_status('not a url') AS bad;
