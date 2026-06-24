-- textplot extension smoke: sparklines, bar charts, QR codes.
SELECT plot_sparkline('[1,2,3,4,5,6,7,8]') AS spark;
SELECT plot_sparkline('not json') AS bad_spark;
SELECT plot_sparkline(NULL) AS null_spark;
SELECT plot_bars('[0,5,10]', 10) AS bars;
SELECT plot_bars('[1,2,3]', 0) AS bad_bars;
SELECT length(qr_utf8('hi')) > 0 AS qr_ok;
SELECT qr_utf8(NULL) AS null_qr;
