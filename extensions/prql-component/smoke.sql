-- prql extension smoke: compile PRQL to SQL, validity checks.
-- Collapse the multi-line SQL to one line so csv-mode output stays single-line.
SELECT replace(prql_to_sql('from employees | filter salary > 100 | select {name}'), chr(10), ' ') AS sql;
SELECT prql_is_valid('from x') AS ok;
SELECT prql_is_valid('!!bad') AS bad;
