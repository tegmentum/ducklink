-- countmin extension smoke: 'a' appears 3x, 'b' 1x, 'z' 0x (never under-counts).
WITH s AS (SELECT count_min(v) cm FROM (VALUES ('a'),('a'),('a'),('b')) t(v))
SELECT cms_estimate(cm, 'a') AS a_freq,
       cms_estimate(cm, 'b') AS b_freq,
       cms_estimate(cm, 'z') AS z_freq
FROM s;
SELECT cms_estimate('00', 'x') AS bad;
