-- braille extension smoke.
SELECT to_braille('hi') AS hi;
SELECT to_braille('abc') AS abc;
SELECT length(to_braille('hello world')) AS len;
