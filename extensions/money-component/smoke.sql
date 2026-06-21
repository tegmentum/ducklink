-- money extension smoke.
SELECT format_money(1234.5, 'USD') AS usd;
SELECT format_money(1000000, 'JPY') AS jpy;
SELECT format_money(-42.5, 'GBP') AS neg;
SELECT format_money(1234567.891, 'EUR') AS eur;
SELECT format_money(1, 'ZZZ') AS bad;
