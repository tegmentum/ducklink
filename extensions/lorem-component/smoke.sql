-- lorem extension smoke (nondeterministic; assert shape).
SELECT array_length(string_split(trim(lorem_words(10)), ' ')) = 10 AS ten_words;
SELECT length(lorem_title()) > 0 AS title_nonempty;
SELECT lorem_words(1) IS NOT NULL AS one_ok;
