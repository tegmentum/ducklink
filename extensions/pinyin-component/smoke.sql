-- pinyin extension smoke: Hanzi -> pinyin romanization.
SELECT to_pinyin('中国') AS tone;
SELECT to_pinyin_plain('中国') AS plain;
SELECT to_pinyin_initials('中国') AS initials;
SELECT to_pinyin('a中') AS passthrough;
SELECT to_pinyin('你好世界') AS hello;
SELECT to_pinyin(NULL) AS nul;
