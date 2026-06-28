-- dplyr smoke: the dplyr(...) pipeline is rewritten by the parser extension
-- (the built-in parser rejects it, the host offers it to dplyr, dplyr
-- transpiles it to SQL). Proves parser dispatch end-to-end.
CREATE TABLE t AS SELECT * FROM (VALUES (1, 10), (2, 20), (3, 30)) AS v(a, b);
dplyr( t |> filter(a == 2) |> select(b) );
