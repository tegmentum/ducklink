-- z85 extension smoke (RFC test vector: 8 bytes 0x86..b9 -> "HelloWorld").
SELECT z85_encode('86 4f d2 6f b5 59 f7 5b' :: text) AS hw_raw;
SELECT z85_encode('864fd26fb559f75b') AS hw;
SELECT z85_decode('HelloWorld') AS bytes;
SELECT z85_encode('abc') AS bad_len;
