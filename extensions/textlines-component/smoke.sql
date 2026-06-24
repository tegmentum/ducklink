-- textlines extension smoke: split_lines(text) -> (line_no, line).
SELECT line_no, line FROM split_lines('alpha' || chr(10) || 'beta' || chr(10) || 'gamma');
SELECT count(*) AS n FROM split_lines('');
