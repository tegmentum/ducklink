-- geohash extension smoke (Empire State Building ~ dr5ru).
SELECT geohash_encode(40.7484, -73.9857, 5) AS gh;
SELECT round(geohash_decode_lat('dr5ru'), 2) AS lat;
SELECT round(geohash_decode_lon('dr5ru'), 2) AS lon;
SELECT geohash_decode_lat('not!valid') AS bad;
