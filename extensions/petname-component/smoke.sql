-- petname extension smoke (nondeterministic; assert shape, not value).
SELECT petname(2, '-') IS NOT NULL AS generated;
SELECT length(petname(3, '-')) > 0 AS nonempty;
SELECT strpos(petname(2, '.'), '.') > 0 AS has_sep;
