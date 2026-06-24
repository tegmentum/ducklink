-- netquack extension smoke (public-suffix-aware URL/domain parsing).
SELECT registrable_domain('https://a.b.example.co.uk/x') AS reg;
SELECT public_suffix('https://a.b.example.co.uk/x') AS suf;
SELECT subdomain('https://a.b.example.co.uk/x') AS sub;
SELECT domain_label('https://a.b.example.co.uk/x') AS label;
SELECT registrable_domain('www.example.com') AS reg2;
SELECT subdomain('example.com') AS sub_empty;
SELECT registrable_domain('co.uk') AS bad;
