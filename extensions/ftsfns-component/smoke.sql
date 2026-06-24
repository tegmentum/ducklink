-- ftsfns extension smoke: tokenize, stem, stem_text, bm25_score, fts_match.
SELECT fts_tokenize('The Quick, brown-fox!') AS toks;
SELECT fts_stem('running', 'english') AS stemmed;
SELECT fts_stem('running', 'klingon') AS bad_lang;
SELECT fts_stem_text('running foxes') AS stems;
SELECT round(bm25_score(1, 1, 10.0, 10.0, 1), 4) AS score;
SELECT fts_match('the quick brown foxes', 'fox') AS m_true;
SELECT fts_match('the quick brown foxes', 'cat') AS m_false;
SELECT fts_tokenize(NULL) AS null_in;
