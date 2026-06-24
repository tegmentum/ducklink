-- polyline extension smoke (canonical Google Encoded Polyline example).
-- Coordinates are JSON [[lon, lat], ...] pairs (lon first, geo-types Coord order).
SELECT polyline_encode('[[-120.2,38.5],[-120.95,40.7],[-126.453,43.252]]', 5) AS enc;
SELECT polyline_decode('_p~iF~ps|U_ulLnnqC_mqNvxq`@', 5) AS dec;
SELECT polyline_decode(polyline_encode('[[-120.2,38.5],[-120.95,40.7],[-126.453,43.252]]', 5), 5) AS roundtrip;
SELECT polyline_encode('not json', 5) AS bad_enc;
SELECT polyline_decode('', 5) AS empty_dec;
