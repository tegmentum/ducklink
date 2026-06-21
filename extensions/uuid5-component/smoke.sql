-- uuid5 extension smoke (RFC 4122 v5 DNS www.example.com -> 2ed6657d-...).
SELECT uuid_v5('dns', 'www.example.com') AS v5_dns;
SELECT uuid_v5('dns', 'www.example.com') = uuid_v5('dns', 'www.example.com') AS deterministic;
SELECT uuid_v3('dns', 'www.example.com') AS v3_dns;
SELECT uuid_v5('not-a-namespace', 'x') AS bad;
