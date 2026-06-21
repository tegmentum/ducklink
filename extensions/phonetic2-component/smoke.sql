-- phonetic2 extension smoke.
SELECT nysiis('Robert') AS r_nysiis;
SELECT refined_soundex('Robert') AS r_refined;
SELECT double_metaphone('Thompson') AS thompson;
SELECT nysiis('Catherine') = nysiis('Katherine') AS name_match;
