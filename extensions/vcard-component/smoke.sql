-- vcard extension smoke. The REPL collapses literal newlines inside string
-- literals, so the VCARD is assembled with chr(10) line joins.
SELECT vcard_count(
  'BEGIN:VCARD' || chr(10) ||
  'VERSION:3.0' || chr(10) ||
  'FN:Alice Smith' || chr(10) ||
  'EMAIL:alice@example.com' || chr(10) ||
  'END:VCARD'
) AS cnt;
SELECT vcard_names(
  'BEGIN:VCARD' || chr(10) ||
  'VERSION:3.0' || chr(10) ||
  'FN:Alice Smith' || chr(10) ||
  'EMAIL:alice@example.com' || chr(10) ||
  'END:VCARD'
) AS names;
SELECT vcard_to_json(
  'BEGIN:VCARD' || chr(10) ||
  'VERSION:3.0' || chr(10) ||
  'FN:Alice Smith' || chr(10) ||
  'EMAIL:alice@example.com' || chr(10) ||
  'ORG:Acme Inc.' || chr(10) ||
  'TEL:+1-555-1234' || chr(10) ||
  'END:VCARD'
) AS j;
SELECT vcard_count('not a vcard') AS bad;
