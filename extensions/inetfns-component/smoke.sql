-- inetfns extension smoke (DuckDB inet scalar surface).
SELECT host('192.168.0.5/24') AS h;
SELECT family('192.168.0.5/24') AS f4;
SELECT family('2001:db8::1/64') AS f6;
SELECT netmask('192.168.0.5/24') AS nm;
SELECT network('192.168.0.5/24') AS net;
SELECT broadcast('192.168.0.5/24') AS bc;
SELECT inet_contains('192.168.0.0/24', '192.168.0.5') AS in_net;
SELECT inet_contains('192.168.0.0/24', '192.168.1.5') AS out_net;
SELECT host('not.an.ip') AS bad;
