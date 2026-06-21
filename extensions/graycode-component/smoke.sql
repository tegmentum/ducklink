-- graycode extension smoke (gray_encode: 0,1,2,3 -> 0,1,3,2).
SELECT gray_encode(0) AS g0;
SELECT gray_encode(2) AS g2;
SELECT gray_encode(3) AS g3;
SELECT gray_decode(gray_encode(170)) AS roundtrip;
SELECT gray_encode(-1) AS neg;
