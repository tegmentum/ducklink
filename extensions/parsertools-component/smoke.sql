-- parsertools extension smoke (sqlparser-rs, generic dialect).
SELECT sql_tables('SELECT * FROM a JOIN b ON a.id=b.id') AS tables;
SELECT sql_statement_type('INSERT INTO t VALUES (1)') AS kind;
SELECT sql_is_valid('SELECT 1') AS ok;
SELECT sql_is_valid('!!!') AS bad;
