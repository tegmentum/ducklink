-- atbash extension smoke (self-inverse; 'abc' -> 'zyx').
SELECT atbash('abc') AS enc;
SELECT atbash('Hello, World!') AS hello;
SELECT atbash(atbash('roundtrip')) AS rt;
