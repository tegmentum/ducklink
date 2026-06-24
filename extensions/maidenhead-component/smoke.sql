-- maidenhead extension smoke (Munich ~ 48.14, 11.60 -> JN58td at precision 3).
SELECT to_maidenhead(48.14, 11.60, 3) AS grid;
SELECT round(maidenhead_lat('JN58td'), 1) AS lat;
SELECT round(maidenhead_lon('JN58td'), 1) AS lon;
SELECT to_maidenhead(200.0, 0.0, 3) AS bad_lat;
SELECT maidenhead_lat('not!valid') AS bad_grid;
