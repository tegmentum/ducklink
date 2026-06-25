-- tpcdsgen extension smoke: TPC-DS reference-data table functions.
--
-- tpcds_income_band() emits the 20 FIXED income bands. Row count must be 20;
-- band 1 is (0, 10000); band 20 is (190001, 200000).
SELECT count(*) AS band_rows FROM tpcds_income_band();
SELECT ib_income_band_sk, ib_lower_bound, ib_upper_bound
FROM tpcds_income_band()
WHERE ib_income_band_sk IN (1, 20)
ORDER BY ib_income_band_sk;

-- tpcds_date_dim_sample() emits a deterministic 366-row slice of date_dim for
-- the leap year 2000. d_date_sk is the Julian Day Number; 2000-01-01 = 2451545.
SELECT count(*) AS date_rows FROM tpcds_date_dim_sample();
SELECT d_date_sk, d_date, d_year, d_moy, d_dom
FROM tpcds_date_dim_sample()
WHERE d_date IN ('2000-01-01', '2000-02-29', '2000-12-31')
ORDER BY d_date_sk;
