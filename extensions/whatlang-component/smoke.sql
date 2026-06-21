-- whatlang extension smoke.
SELECT detect_language('I love programming and reading good books') AS eng;
SELECT detect_language_name('Ich liebe die deutsche Sprache wirklich sehr') AS deu_name;
SELECT detect_script('これは日本語のテキストです') AS jp_script;
