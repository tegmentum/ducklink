-- dns extension smoke (network; assert shape/resolvability, not exact IPs).
SELECT dns_lookup('localhost') AS localhost_ip;
SELECT dns_lookup('one.one.one.one') IS NOT NULL AS cloudflare_resolves;
SELECT json_array_length(dns_resolve_all('localhost')) >= 1 AS has_ips;
SELECT dns_lookup('nonexistent.invalid.') AS nxdomain;
