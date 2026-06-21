-- hashids extension smoke.
SELECT hashids_decode(hashids_encode(12345, 'salt'), 'salt') AS roundtrip;
SELECT hashids_encode(1, 'salt') = hashids_encode(1, 'other') AS salt_matters;
SELECT hashids_decode('not!valid', 'salt') AS bad;
