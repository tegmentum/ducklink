-- iso extension smoke.
SELECT iso_country_name('US') AS us;
SELECT iso_country_alpha3('de') AS de3;
SELECT iso_country_numeric('JP') AS jp_num;
SELECT iso_country_name('ZZ') AS bad;
