-- checksums extension smoke (CRC-16/ARC of "123456789" = 0xBB3D = 47933).
SELECT crc16('123456789') AS crc16_check;
SELECT adler32('Wikipedia') AS adler;
SELECT fnv1a_32('') AS fnv32_empty;
SELECT fnv1a_64('a') AS fnv64_a;
SELECT crc16(NULL) AS nullarg;
