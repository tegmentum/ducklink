SELECT ST_Transform('POINT(-122.4194 37.7749)', 4326, 3857) AS web_mercator;
SELECT ST_Transform('POINT(0 0)', 4326, 3857) AS origin;
SELECT ST_Transform('POINT(-122.4194 37.7749)', 4326, 0) AS bad;
