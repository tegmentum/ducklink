-- hocon extension smoke: dotted-path lookup + HOCON->JSON over a tiny config.
SELECT hocon_get('db { host = localhost, port = 5432 }', 'db.host') AS host;
SELECT hocon_get('db { host = localhost, port = 5432 }', 'db.port') AS port;
SELECT hocon_to_json('db { host = localhost, port = 5432 }') AS j;
SELECT hocon_get('db { host = localhost }', 'db.missing') AS absent;
SELECT hocon_to_json('db { host = ') AS bad;
