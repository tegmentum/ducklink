-- simhash extension smoke (similar texts -> small distance; stable fingerprint).
SELECT simhash('the quick brown fox') = simhash('the quick brown fox') AS stable;
SELECT simhash_distance('the quick brown fox', 'the quick brown fox') AS identical;
SELECT simhash_distance('the quick brown fox', 'the quick brown dog') < 20 AS similar_close;
SELECT simhash_distance('apple banana cherry', 'xyz qrs tuv') > 10 AS different_far;
