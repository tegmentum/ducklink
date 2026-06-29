-- a5 extension smoke: the A5 pentagonal DGGS global geospatial index.
-- Cell ids are nondeterministic across versions, so assert via round-trips.
-- An encoded point yields a valid cell.
SELECT a5_is_valid_cell(a5_lonlat_to_cell(40.7484, -73.9857, 10)) AS valid;
-- Resolution echoes back.
SELECT a5_cell_to_resolution(a5_lonlat_to_cell(0.0, 0.0, 5)) AS res;
-- Decoding a cell center returns (approximately) the encoded point.
SELECT round(a5_cell_to_lat(a5_lonlat_to_cell(40.0, -73.0, 10))) AS lat;
SELECT round(a5_cell_to_lon(a5_lonlat_to_cell(40.0, -73.0, 10))) AS lon;
-- hex <-> cell round-trips.
SELECT a5_hex_to_cell(a5_cell_to_hex(a5_lonlat_to_cell(1.0, 2.0, 7)))
       = a5_lonlat_to_cell(1.0, 2.0, 7) AS hex_roundtrip;
-- Bad hex -> NULL.
SELECT a5_hex_to_cell('not-hex') AS bad_hex;
-- A parent cell sits at the requested coarser resolution.
SELECT a5_cell_to_resolution(a5_cell_to_parent(a5_lonlat_to_cell(51.5074, -0.1278, 8), 5)) AS parent_res;
