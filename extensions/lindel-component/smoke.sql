-- lindel extension smoke: space-filling curve round-trips.
-- Morton (Z-order): encode (5,3) -> 27, decode back to 5 and 3.
SELECT morton_encode(5, 3) AS z;
SELECT morton_decode_x(27) AS mx;
SELECT morton_decode_y(27) AS my;
-- Hilbert: encode (5,3) -> 28, decode back to 5 and 3.
SELECT hilbert_encode(5, 3) AS h;
SELECT hilbert_decode_x(28) AS hx;
SELECT hilbert_decode_y(28) AS hy;
-- Out-of-range / negative input -> NULL.
SELECT morton_encode(-1, 3) AS bad;
