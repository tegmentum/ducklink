-- Smoke test for the `textdiff` extension (loaded by the harness). `.mode csv`.
SELECT round(diff_ratio('hello world', 'hello there'), 2) AS ratio;
SELECT diff_changed_lines('a' || chr(10) || 'b', 'a' || chr(10) || 'c') AS changed;
SELECT text_diff('x', 'x') AS identical;
SELECT diff_ratio(NULL, 'x') AS null_in;
