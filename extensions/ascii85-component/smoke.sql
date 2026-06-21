-- ascii85 extension smoke.
SELECT ascii85_encode('Hello') AS enc;
SELECT ascii85_decode(ascii85_encode('round trip!')) AS roundtrip;
SELECT ascii85_decode('!!!bad!!!') AS bad;
