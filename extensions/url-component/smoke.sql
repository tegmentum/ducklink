-- url extension smoke (loaded by harness; .mode csv).
SELECT url_scheme('https://user@ex.com:8443/p/q?a=1#f') AS scheme;
SELECT url_host('https://ex.com:8443/p?a=1') AS host;
SELECT url_port('https://ex.com/p') AS default_port;
SELECT url_path('https://ex.com/a/b/c') AS path;
SELECT url_query('https://ex.com/p?a=1&b=2') AS query;
SELECT url_host('not a url') AS bad;
