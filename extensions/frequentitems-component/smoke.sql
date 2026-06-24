-- frequentitems extension smoke: top-K heavy hitters over a JSON array.
-- Counts: a=3, b=2, c=1; top 2 by count desc, ties by first-seen order.
SELECT top_k('["a","a","a","b","b","c"]', 2) AS tk;
SELECT top_k_value('["a","a","a","b","b","c"]', 2) AS tkv;
-- NULL JSON input -> NULL.
SELECT top_k(NULL, 2) AS nul;
-- k <= 0 -> NULL.
SELECT top_k('["a","b"]', 0) AS kzero;
-- Bad JSON -> NULL.
SELECT top_k('not json', 2) AS badjson;
