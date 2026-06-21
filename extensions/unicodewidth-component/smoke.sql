-- unicodewidth extension smoke (CJK chars are width 2; flag emoji is 1 grapheme).
SELECT grapheme_count('cafe' || chr(769)) AS graphemes;
SELECT length('cafe' || chr(769)) AS codepoints;
SELECT display_width('hello') AS w_ascii;
SELECT display_width('你好') AS w_cjk;
