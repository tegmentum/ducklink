-- marisa extension smoke: fst-backed set lookup / prefix search.
SELECT fst_contains('["apple","apply","banana","band"]', 'apple') AS has_apple;
SELECT fst_contains('["apple","apply","banana","band"]', 'grape') AS has_grape;
SELECT fst_prefix('["apple","apply","banana","band"]', 'app') AS app_terms;
SELECT fst_prefix('["apple","apply","banana","band"]', 'ban') AS ban_terms;
SELECT fst_count('["apple","apply","banana","band","apple"]') AS n;
SELECT fst_contains(NULL, 'apple') AS null_in;
