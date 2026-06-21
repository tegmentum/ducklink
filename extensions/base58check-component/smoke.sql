-- base58check extension smoke.
SELECT base58check_encode('00') AS enc;
SELECT base58check_decode(base58check_encode('deadbeef')) AS roundtrip;
SELECT base58check_decode('bad0OIl') AS bad;
