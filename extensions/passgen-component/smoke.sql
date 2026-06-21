-- passgen extension smoke (nondeterministic; assert shape).
SELECT length(gen_password(20)) AS len20;
SELECT length(gen_password_alnum(12)) AS len12;
SELECT gen_password(8) <> gen_password(8) AS distinct_each;
SELECT gen_password_alnum(10) ~ '^[A-Za-z0-9]+$' AS alnum_only;
