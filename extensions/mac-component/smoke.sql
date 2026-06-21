-- mac extension smoke.
SELECT mac_valid('01:23:45:67:89:ab') AS ok;
SELECT mac_valid('zz:zz:zz:zz:zz:zz') AS bad;
SELECT mac_normalize('01-23-45-67-89-ab') AS norm;
SELECT mac_normalize('garbage') AS bad_norm;
