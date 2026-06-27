-- aggstat extension smoke (harmonic mean of 1,2,4 = 3/(1+0.5+0.25) = 1.714...).
SELECT round(harmonic_mean(x), 6) AS hm FROM (SELECT 1 x UNION ALL SELECT 2 UNION ALL SELECT 4) t;
SELECT round(harmonic_mean(x), 1) AS hm2 FROM (SELECT 2.0 x UNION ALL SELECT 8.0) t;
SELECT harmonic_mean(x) AS empty FROM (SELECT 1 x WHERE false) t;
