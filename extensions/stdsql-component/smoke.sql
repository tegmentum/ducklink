-- stdsql: only the cross-dialect scalars DuckDB does NOT ship as builtins.
-- (greatest/least/left/right/lpad/rpad/repeat/translate/to_hex/bit_length/
--  chr/ascii/char_length/character_length/from_hex/get_bit/set_bit are DuckDB
--  builtins -- not re-registered; `if` stays DuckDB's CASE syntax.)
SELECT length(space(3)) AS sp;
SELECT initcap('hello world') AS ic;
-- ClickHouse camelCase family
SELECT startsWith('foobar', 'foo') AS sw;
SELECT endsWith('foobar', 'baz') AS ew;
SELECT lengthUTF8('hello') AS lu;
SELECT lowerUTF8('ABC') AS lo;
SELECT upperUTF8('abc') AS up;
SELECT toString('42') AS ts;
SELECT empty('') AS em;
SELECT notEmpty('x') AS ne;
SELECT replaceAll('a.b.c', '.', '-') AS ra;
SELECT positionUTF8('abcdef', 'cd') AS p1;
SELECT positionUTF8('abc', 'z') AS p0;
-- PostgreSQL to_* / quote_* / byte accessors
SELECT to_bin(5) AS tb;
SELECT to_oct(8) AS toc;
SELECT to_ascii('cafe!') AS ta;
SELECT quote_literal('x') AS ql;
SELECT quote_nullable('y') AS qn;
-- 0x01020304 = 16909060: byte 0 = 4; set byte 0 to 255 -> 0x010203ff = 16973823
SELECT get_byte(16909060, 0) AS gb;
SELECT set_byte(16909060, 0, 255) AS sb;
