-- ggsql smoke: the VISUALIZE statement is rewritten by the parser extension
-- (the built-in parser rejects it, the host offers it to ggsql, ggsql rewrites
-- it into a text-bar-chart SELECT). Proves parser dispatch end-to-end.
VISUALIZE SELECT 'apple' AS label, 3 AS n UNION ALL SELECT 'pear', 1;
