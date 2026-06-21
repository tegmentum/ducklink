-- slug extension smoke.
SELECT slugify('Hello, World!') AS s1;
SELECT slugify('  Crème Brûlée  ') AS s2;
SELECT slugify('Foo___Bar  Baz') AS s3;
