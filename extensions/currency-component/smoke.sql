-- currency extension smoke.
SELECT currency_name('USD') AS usd;
SELECT currency_numeric('EUR') AS eur_num;
SELECT currency_exponent('JPY') AS jpy_exp;
SELECT currency_exponent('USD') AS usd_exp;
SELECT currency_name('ZZZ') AS bad;
