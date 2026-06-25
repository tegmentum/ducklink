-- geomtype: a COHERENT custom logical type. `geom2` (BLOB-physical, WKB) with
-- VARCHAR<->geom2 casts (WKT<->WKB) and typed scalars over it. A geom2 column
-- round-trips through WKT via the registered casts.
-- (`geometry` is a DuckDB v1.5.x BUILTIN type; we register a fresh name `geom2`
--  to prove the NEW custom register-logical-type + register-cast surface.)
CREATE TABLE g(shape geom2);
-- VARCHAR -> geom2 cast (WKT parse -> WKB blob) fires on the explicit ::geom2.
INSERT INTO g VALUES ('POLYGON((0 0,0 2,2 2,2 0,0 0))'::geom2);
-- geom2 -> VARCHAR cast (WKB -> WKT) fires on shape::VARCHAR; geom_area is the
-- typed scalar consuming the geom2 (WKB) value: a 2x2 box has area 4.
SELECT shape::VARCHAR AS wkt, geom_area(shape) AS area FROM g;
-- geom_astext is a second typed scalar; same WKB->WKT path inside the function.
SELECT geom_astext(shape) AS astext FROM g;
-- A point round-trips too (different WKB type code).
SELECT ('POINT(3 7)'::geom2)::VARCHAR AS pt;
