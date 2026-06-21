-- vigenere extension smoke (classic: ATTACKATDAWN / LEMON -> LXFOPVEFRNHR).
SELECT vigenere_encrypt('ATTACKATDAWN', 'LEMON') AS enc;
SELECT vigenere_decrypt('LXFOPVEFRNHR', 'LEMON') AS dec;
SELECT vigenere_decrypt(vigenere_encrypt('Hello, World!', 'key'), 'key') AS roundtrip;
SELECT vigenere_encrypt('abc', '') AS empty_key;
