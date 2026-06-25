-- hnswfns custom-index smoke (Item 3 / M2a): build a real HNSW from a table's
-- FLOAT[3] rows via CREATE INDEX ... USING wasm_hnsw, then run an explicit kNN
-- search and cross-check against brute-force array_distance.
CREATE TABLE v(id BIGINT, e FLOAT[3]);
INSERT INTO v VALUES (1,[1,2,3]),(2,[4,5,6]),(3,[1,1,1]),(4,[9,9,9]);
CREATE INDEX h ON v USING wasm_hnsw (e);
-- HNSW kNN: the 2 nearest rows to [1,2,2] (rowid is 0-based; row 1 -> rowid 0).
SELECT rowid, round(distance, 3) AS d FROM hnsw_search('h', '[1,2,2]', 2) ORDER BY d;
-- Brute-force cross-check: same rows, same order.
SELECT id, round(array_distance(e, [1,2,2]::FLOAT[3]), 3) AS d FROM v ORDER BY d LIMIT 2;
