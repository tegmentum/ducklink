-- tpchgen smoke: deterministic TPC-H reference tables + scaled tables + query text.
--
-- tpch_region() is the fixed 5-row REGION table; assert its size and that
-- regionkey 0 is AFRICA.
SELECT count(*) AS region_rows FROM tpch_region();
SELECT r_name FROM tpch_region() WHERE r_regionkey = 0;
--
-- tpch_nation() is the fixed 25-row NATION table; nation 0 is ALGERIA in
-- region 0 (AFRICA).
SELECT count(*) AS nation_rows FROM tpch_nation();
SELECT n_name, n_regionkey FROM tpch_nation() WHERE n_nationkey = 0;
--
-- Scaled tables at sf=0.01: customer/supplier/part have fixed base counts
-- scaled linearly (150000/10000/200000 * 0.01).
SELECT count(*) AS customer_rows FROM tpch_customer(0.01);
SELECT count(*) AS supplier_rows FROM tpch_supplier(0.01);
SELECT count(*) AS part_rows FROM tpch_part(0.01);
--
-- lineitem at a tiny sf produces a nonzero, bounded number of rows.
SELECT count(*) > 0 AS lineitem_nonempty FROM tpch_lineitem(0.001);
--
-- NULL / non-positive scale factor -> zero rows (never a panic).
SELECT count(*) AS null_sf_rows FROM tpch_lineitem(NULL);
SELECT count(*) AS zero_sf_rows FROM tpch_orders(0.0);
--
-- tpch_query(n) returns the SQL text of TPC-H query n; query 1 mentions
-- lineitem. Out-of-range -> NULL.
SELECT lower(tpch_query(1)) LIKE '%lineitem%' AS q1_has_lineitem;
SELECT tpch_query(0) IS NULL AS q0_is_null;
SELECT tpch_query(23) IS NULL AS q23_is_null;
