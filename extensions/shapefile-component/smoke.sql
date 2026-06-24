-- shapefile extension smoke: read_shp(data BLOB) -> (shape_no, shape_type, wkt).
--
-- Fixture: a 2-shape ESRI .shp stream (geometry only -- no .shx/.dbf), generated
-- by the `shapefile` crate's ShapeWriter (see fixture note in smoke.expected):
--   Point(1,2) and Point(3,4)
-- => (1,'Point','POINT(1 2)'), (2,'Point','POINT(3 4)').
SELECT shape_no, shape_type, wkt
FROM read_shp(unhex('0000270A00000000000000000000000000000000000000000000004EE803000001000000000000000000F03F0000000000000040000000000000084000000000000010400000000000000000000000000000000000000000000000000000000000000000000000010000000A01000000000000000000F03F0000000000000040000000020000000A0100000000000000000008400000000000001040'))
ORDER BY shape_no;

-- Malformed blob -> zero rows (proves the function is wired, never panics).
SELECT count(*) AS bad_rows FROM read_shp('not a shp'::BLOB);

-- Empty blob -> zero rows.
SELECT count(*) AS empty_rows FROM read_shp(''::BLOB);

-- Real-file usage: a shapefile dataset on disk is foo.shp + foo.shx + foo.dbf.
-- read_shp reads geometry from the .shp stream only; load its bytes as a BLOB, e.g.
--   SELECT * FROM read_shp((SELECT content FROM read_blob('foo.shp')));
