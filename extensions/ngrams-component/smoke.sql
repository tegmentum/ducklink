-- ngrams extension smoke.
SELECT char_ngrams('hello', 2) AS bigrams;
SELECT word_ngrams('the quick brown fox', 2) AS word_bigrams;
SELECT char_ngrams('hi', 5) AS too_long;
SELECT json_array_length(char_ngrams('abcdef', 3)) AS count;
