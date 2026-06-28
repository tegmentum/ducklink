-- prql-parser smoke: a PRQL pipeline is rewritten by the parser extension
-- (the built-in parser rejects it, the host offers it to prql, prqlc compiles
-- it to SQL). Proves parser dispatch end-to-end.
CREATE TABLE inv AS SELECT * FROM (VALUES (1, 150), (2, 50), (3, 200)) AS v(id, total);
from inv | filter total > 100 | select {id, total};
