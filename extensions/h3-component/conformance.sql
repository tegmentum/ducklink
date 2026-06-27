-- h3 extension smoke (San Francisco: lat=37.775, lng=-122.418, res=9).
SELECT h3_latlng_to_cell(37.775, -122.418, 9) AS cell;
SELECT h3_is_valid_cell(h3_latlng_to_cell(37.775, -122.418, 9)) AS valid;
SELECT h3_is_valid_cell(123) AS bad;
SELECT round(h3_cell_to_lat(h3_latlng_to_cell(37.775, -122.418, 9)), 3) AS lat;
SELECT round(h3_cell_to_lng(h3_latlng_to_cell(37.775, -122.418, 9)), 3) AS lng;
SELECT h3_cell_to_parent(h3_latlng_to_cell(37.775, -122.418, 9), 5) AS parent;
SELECT h3_grid_distance(h3_latlng_to_cell(37.775, -122.418, 9), h3_latlng_to_cell(37.775, -122.418, 9)) AS dist0;
SELECT h3_latlng_to_cell(NULL, -122.418, 9) AS null_in;
