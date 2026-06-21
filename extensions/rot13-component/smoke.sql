-- rot13 extension smoke.
SELECT rot13('Hello, World!') AS enc;
SELECT rot13(rot13('roundtrip')) AS roundtrip;
SELECT caesar('abc', 1) AS shift1;
SELECT caesar('XYZ', 3) AS wrap;
