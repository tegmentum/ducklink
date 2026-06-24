-- vssfns: non-core vector math (L2/cosine/dot are already in core_functions).
-- Vectors are JSON number arrays in VARCHAR. Comments on their own lines only.
SELECT vec_l1_distance('[1, 2, 3]', '[4, 6, 3]') AS l1;
SELECT vec_linf_distance('[1, 2, 3]', '[4, 6, 3]') AS linf;
SELECT vec_l1_distance('[1, 2, 3]', '[1, 2]') AS mismatch;
SELECT vec_l1_distance('garbage', '[1, 2]') AS bad;
SELECT vec_normalize('[3, 4]') AS unit;
SELECT vec_normalize('[0, 0, 0]') AS zero;
