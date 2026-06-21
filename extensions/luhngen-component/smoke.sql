-- luhngen extension smoke (7992739871 -> check digit 3 -> 79927398713).
SELECT luhn_check_digit('7992739871') AS cd;
SELECT luhn_append('7992739871') AS full;
SELECT luhn_check_digit('') AS empty;
