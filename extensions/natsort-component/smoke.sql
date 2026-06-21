-- natsort extension smoke (natural order: img2 < img12).
SELECT natsort_compare('img2', 'img12') AS lt;
SELECT natsort_compare('img12', 'img2') AS gt;
SELECT natsort_compare('file10', 'file10') AS eq;
SELECT natsort_compare('a100', 'a99') AS gt2;
