-- checkdigit extension smoke (Verhoeff of 236 -> check 3 => 2363 valid; Damm of 572 -> 4 => 5724 valid).
SELECT verhoeff_append('236') AS v_app;
SELECT verhoeff_validate('2363') AS v_ok;
SELECT verhoeff_validate('2364') AS v_bad;
SELECT damm_append('572') AS d_app;
SELECT damm_validate('5724') AS d_ok;
SELECT damm_validate('5720') AS d_bad;
