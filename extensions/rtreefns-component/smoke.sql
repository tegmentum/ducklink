-- rtreefns custom spatial R-tree index (Item 3 / deferred spatial keystone):
-- proves the SAME index WIT that backs HNSW (hnswfns) backs a spatial R-tree
-- with ZERO core change. The indexed column is a FLOAT[4] bbox (minx,miny,maxx,
-- maxy) per row -- the SAME FLOAT[N] ingest path HNSW uses -- and the search
-- interprets the list<f32> payload as a query bbox instead of a point vector.
CREATE TABLE g(id BIGINT, bb FLOAT[4]);
INSERT INTO g VALUES (1,[0,0,1,1]),(2,[5,5,6,6]),(3,[0.5,0.5,2,2]),(4,[10,10,11,11]);
CREATE INDEX r ON g USING wasm_rtree (bb);
-- Spatial intersection: rows whose bbox INTERSECTS [0,0,1.5,1.5] are row 1
-- ([0,0,1,1]) and row 3 ([0.5,0.5,2,2]) -> rowids 0 and 2 (0-based). Rows 2
-- ([5,5,6,6]) and 4 ([10,10,11,11]) do NOT intersect and are excluded.
SELECT rowid FROM rtree_search('r', '[0,0,1.5,1.5]', 0) ORDER BY rowid;
-- bbox4 helper: bounding box of a WKT polygon as JSON '[minx,miny,maxx,maxy]'.
SELECT bbox4('POLYGON((0 0, 2 0, 2 3, 0 3, 0 0))') AS bb;
