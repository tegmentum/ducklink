-- ipaddr extension smoke.
SELECT ip_valid('192.168.1.1') AS ok4;
SELECT ip_valid('2001:db8::1') AS ok6;
SELECT ip_valid('999.1.1.1') AS bad;
SELECT ip_version('::1') AS v6;
SELECT ip_is_private('10.0.0.5') AS priv;
SELECT ip_is_private('8.8.8.8') AS pub;
