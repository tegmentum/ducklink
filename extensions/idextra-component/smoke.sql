-- idextra extension smoke (nondeterministic; assert shape/length).
-- ksuid is 27 chars (base62), cuid2 defaults to 24.
SELECT length(ksuid()) AS ksuid_len;
SELECT length(cuid2()) > 0 AS cuid2_nonempty;
SELECT ksuid() <> ksuid() AS distinct_each;
SELECT cuid2() ~ '^[a-z0-9]+$' AS cuid2_alnum;
