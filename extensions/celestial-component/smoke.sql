-- celestial extension smoke (astronomical coordinate conversions).
-- Pole-to-pole separation is exactly 90 degrees.
SELECT round(angular_separation(0, 0, 0, 90), 1) AS sep;
-- 12h of RA == 180 degrees.
SELECT round(hms_to_deg(12, 0, 0), 1) AS ra_deg;
-- 41 deg 30' == 41.5 deg.
SELECT round(dms_to_deg(41, 30, 0), 1) AS dms;
-- Galactic center (Sgr A*, RA=266.405, Dec=-28.936) maps to l~0, b~0.
SELECT round(equatorial_to_galactic_l(266.405, -28.936), 1) AS gal_l;
SELECT round(equatorial_to_galactic_b(266.405, -28.936), 1) AS gal_b;
-- NULL input propagates to NULL.
SELECT angular_separation(NULL, 0, 0, 90) AS sep_null;
