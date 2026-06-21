-- stopwords extension smoke.
SELECT is_stopword('the', 'english') AS the_is;
SELECT is_stopword('elephant', 'english') AS elephant_not;
SELECT remove_stopwords('the quick brown fox is on the run', 'en') AS stripped;
SELECT is_stopword('le', 'french') AS french;
SELECT is_stopword('x', 'klingon') AS bad_lang;
