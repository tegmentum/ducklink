-- gravatar extension smoke (md5 of trimmed/lowercased email).
SELECT gravatar_hash('  MyEmailAddress@example.com ') AS hash;
SELECT gravatar_url('myemailaddress@example.com') AS url;
