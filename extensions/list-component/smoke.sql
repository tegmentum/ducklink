-- list: only the JSON-array scalars DuckDB does NOT ship as builtins.
-- (array_append/prepend/cat/concat/length/position/to_string/slice/sort/
--  distinct/contains/reverse/flatten/len + the list_* native-LIST family +
--  list_min/max/sum/product/avg/count + array_intersect/list_intersect +
--  array_to_json are DuckDB builtins -- not re-registered.)
-- Carrier is TEXT containing a JSON array.
-- element values are JSON-encoded TEXT (the carrier contract): DuckDB does
-- not implicitly cast an INTEGER literal to VARCHAR, so pass '2' not 2.
SELECT array_remove('[1,2,3,2,4,2]', '2') AS rm;
SELECT list_length('[1,2,3,4,5]') AS len;
SELECT list_length('[]') AS len0;
-- numeric reductions (cast DOUBLE -> BIGINT for format-stable output)
SELECT CAST(array_sum('[1,2,3,4]') AS BIGINT) AS sum;
SELECT CAST(array_product('[1,2,3,4]') AS BIGINT) AS prod;
SELECT CAST(array_min('[3,1,4,1,5]') AS BIGINT) AS mn;
SELECT CAST(array_max('[3,1,4,1,5]') AS BIGINT) AS mx;
SELECT CAST(array_avg('[2,4,6]') AS BIGINT) AS avg;
SELECT array_count('[1,null,3,null,5]') AS cnt;
-- dimension introspection
SELECT array_dims('[10,20,30]') AS dims;
SELECT array_lower('[10,20,30]') AS lo;
SELECT array_upper('[10,20,30]') AS hi;
SELECT array_ndims('[10,20,30]') AS nd;
-- positions / replace / overlap
SELECT array_positions('[5,6,5,7,5]', '5') AS poss;
SELECT array_replace('[1,2,3,2]', '2', '9') AS rep;
SELECT arrays_overlap('[1,2,3]', '[3,4,5]') AS ov1;
SELECT arrays_overlap('[1,2,3]', '[7,8,9]') AS ov0;
-- mixed-type element via JSON encoding
SELECT array_remove('[1,2,"three",2]', '"three"') AS rmstr;
