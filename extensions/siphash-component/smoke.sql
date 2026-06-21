-- siphash extension smoke (deterministic given keys).
SELECT siphash(0, 0, 'hello') AS h_zero_keys;
SELECT siphash(1, 2, 'hello') AS h_keyed;
SELECT siphash(0, 0, 'hello') = siphash(0, 0, 'hello') AS stable;
SELECT siphash(0, 0, 'a') = siphash(0, 0, 'b') AS distinct_inputs;
