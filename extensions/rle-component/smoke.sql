-- rle extension smoke.
SELECT rle_encode('aaabbc') AS enc;
SELECT rle_decode('3a2b1c') AS dec;
SELECT rle_decode(rle_encode('wwwwaaadexxxxxx')) AS roundtrip;
SELECT rle_decode('abc') AS bad;
