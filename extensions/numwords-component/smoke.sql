-- numwords extension smoke.
SELECT num_to_words(123) AS n123;
SELECT num_to_words(1000000) AS million;
SELECT num_to_ordinal_words(21) AS ord21;
SELECT num_to_words(-5) AS neg;
