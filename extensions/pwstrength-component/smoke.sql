-- pwstrength extension smoke (weak << strong).
SELECT password_strength('123456') AS weak_label;
SELECT password_strength('Tr0ub4dour&3xpl!Zq') AS strong_label;
SELECT password_score('123456') < password_score('Tr0ub4dour&3xpl!Zq') AS ordered;
SELECT password_score('') AS empty;
