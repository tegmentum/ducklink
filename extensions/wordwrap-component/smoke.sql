-- wordwrap extension smoke (\n shows as the two chars backslash-n in csv? use length checks).
SELECT word_wrap('the quick brown fox jumps', 10) AS wrapped;
SELECT word_wrap('one two three four five', 9) AS w9;
