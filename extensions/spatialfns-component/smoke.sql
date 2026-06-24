-- spatialfns extension smoke: subset of spatial ST_* over WKT text.
-- ST_Point constructs POINT WKT.
SELECT ST_Point(1.0, 2.0) AS pt;
-- ST_GeomFromText validates/normalizes WKT.
SELECT ST_GeomFromText('POINT(1 2)') AS norm;
-- Invalid WKT -> NULL.
SELECT ST_GeomFromText('NOT A GEOM') AS bad;
-- ST_AsText round-trips.
SELECT ST_AsText('LINESTRING(0 0, 3 4)') AS astext;
-- Point coordinate accessors.
SELECT ST_X('POINT(1 2)') AS x;
SELECT ST_Y('POINT(1 2)') AS y;
-- Euclidean distance: 3-4-5 triangle.
SELECT round(ST_Distance('POINT(0 0)', 'POINT(3 4)'), 3) AS dist;
-- Polygon area of a 2x2 box.
SELECT round(ST_Area('POLYGON((0 0,0 2,2 2,2 0,0 0))'), 3) AS area;
-- Line length of the 3-4-5 leg.
SELECT round(ST_Length('LINESTRING(0 0, 3 4)'), 3) AS len;
-- Polygon perimeter (2x2 box).
SELECT round(ST_Length('POLYGON((0 0,0 2,2 2,2 0,0 0))'), 3) AS perim;
-- Centroid of the 2x2 box is its center.
SELECT ST_Centroid('POLYGON((0 0,0 2,2 2,2 0,0 0))') AS cent;
-- Spatial predicates.
SELECT ST_Contains('POLYGON((0 0,0 4,4 4,4 0,0 0))', 'POINT(1 1)') AS contains;
SELECT ST_Within('POINT(1 1)', 'POLYGON((0 0,0 4,4 4,4 0,0 0))') AS within;
SELECT ST_Intersects('LINESTRING(0 0,4 4)', 'LINESTRING(0 4,4 0)') AS hit;
SELECT ST_Intersects('POINT(0 0)', 'POINT(9 9)') AS miss;
-- Envelope (bounding box) of a line.
SELECT ST_Envelope('LINESTRING(0 0, 3 4)') AS env;
-- GeoJSON output.
SELECT ST_AsGeoJSON('POINT(1 2)') AS gj;
