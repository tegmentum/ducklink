-- numfmt extension smoke: thousands-grouping + SI/metric prefixes.
SELECT num_group(1234567.5, 2) AS grp;
SELECT num_si(1500) AS si_k;
SELECT num_si(2300000) AS si_m;
SELECT num_si(0.0023) AS si_milli;
SELECT num_group(NULL, 2) AS grp_null;
SELECT num_si(NULL) AS si_null;
