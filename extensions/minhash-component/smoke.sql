-- minhash extension smoke: identical sets -> 1.0; disjoint -> ~0; overlap in between.
WITH a AS (SELECT minhash(v) s FROM (VALUES ('a'),('b'),('c'),('d')) t(v)),
     b AS (SELECT minhash(v) s FROM (VALUES ('a'),('b'),('c'),('d')) t(v)),
     c AS (SELECT minhash(v) s FROM (VALUES ('x'),('y'),('z'),('w')) t(v))
SELECT minhash_similarity((SELECT s FROM a), (SELECT s FROM b)) AS identical,
       minhash_similarity((SELECT s FROM a), (SELECT s FROM c)) < 0.4 AS disjoint_low;
SELECT minhash_similarity('00', 'ff') AS bad;
