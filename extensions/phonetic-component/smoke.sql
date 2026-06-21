-- phonetic extension smoke (Soundex: Robert/Rupert -> R163).
SELECT soundex('Robert') AS robert;
SELECT soundex('Rupert') AS rupert;
SELECT metaphone('Thompson') AS thompson;
SELECT soundex('') AS empty;
