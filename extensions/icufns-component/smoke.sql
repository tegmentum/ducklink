-- icufns extension smoke: locale-sensitive collation scalars on the LEAN core.
-- icu_compare: 'a' before 'b' in English.
SELECT icu_compare('a', 'b', 'en') AS cmp_ab;
-- Swedish: 'z' sorts BEFORE 'a-ring' (it is near the end of the Swedish alphabet).
SELECT icu_compare('z', 'ä', 'sv') AS cmp_sv;
-- English: opposite ordering for the same two characters.
SELECT icu_compare('z', 'ä', 'en') AS cmp_en;
-- Sort keys drive ORDER BY in locale order (workaround for real COLLATE).
SELECT name FROM (VALUES ('banana'), ('apple'), ('cherry')) t(name)
  ORDER BY icu_sort_key(name, 'en');
-- Full Unicode case folding: German sharp-s folds to 'ss'.
SELECT icu_casefold('GROẞE STRASSE') AS folded;
-- NULL propagation.
SELECT icu_compare(NULL, 'b', 'en') AS cmp_null;
