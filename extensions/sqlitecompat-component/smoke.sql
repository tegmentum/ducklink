-- sqlitecompat (#153 cross-compat): SQLite built-in scalars DuckDB lacks.
-- (instr/unicode/printf/hex/unhex/typeof/chr/length/replace/substr ARE
--  DuckDB builtins -- not re-registered.) These add the SQLite spellings.
SELECT octet_length(zeroblob(4)) AS zb_len;
SELECT octet_length(randomblob(8)) AS rb_len;
SELECT likely(true) AS lk;
SELECT unlikely(false) AS ulk;
SELECT likelihood(true, 0.75) AS lh;
