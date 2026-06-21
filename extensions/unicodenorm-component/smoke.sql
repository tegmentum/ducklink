-- unicodenorm extension smoke (é as U+00E9 vs e + U+0301 combining).
SELECT nfc('e' || chr(769)) = chr(233) AS composes;
SELECT length(nfd(chr(233))) AS decomposed_len;
SELECT nfkc('ﬁ') AS ligature;
SELECT nfc('plain') AS plain;
