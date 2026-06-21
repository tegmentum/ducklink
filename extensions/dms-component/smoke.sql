-- dms extension smoke (40 26 46 -> 40.446111).
SELECT round(dms_to_decimal(40, 26, 46), 6) AS dec;
SELECT round(dms_to_decimal(-73, 58, 23), 6) AS neg;
SELECT decimal_to_dms(40.446111) AS dms;
SELECT decimal_to_dms(-73.97306) AS dms_neg;
