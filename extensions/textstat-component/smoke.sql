-- textstat extension smoke.
SELECT word_count('The quick brown fox jumps.') AS words;
SELECT sentence_count('One. Two! Three?') AS sentences;
SELECT syllable_count('readability') AS syllables;
SELECT round(reading_time_minutes('one two three four five six seven eight nine ten'), 2) AS minutes;
SELECT flesch_reading_ease('') AS empty;
