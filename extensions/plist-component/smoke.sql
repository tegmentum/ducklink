-- plist extension smoke: a tiny inline XML property list.
-- The document is assembled with chr(10) joins so each statement stays on one
-- SQL line (the CLI runs statement-by-statement, so the literal is repeated).
SELECT plist_get(
  '<?xml version="1.0" encoding="UTF-8"?>' || chr(10) ||
  '<plist version="1.0">' || chr(10) || '<dict>' || chr(10) ||
  '<key>name</key><string>Alice</string>' || chr(10) ||
  '<key>age</key><integer>30</integer>' || chr(10) ||
  '</dict>' || chr(10) || '</plist>', 'name') AS name;
SELECT plist_get(
  '<?xml version="1.0" encoding="UTF-8"?>' || chr(10) ||
  '<plist version="1.0">' || chr(10) || '<dict>' || chr(10) ||
  '<key>name</key><string>Alice</string>' || chr(10) ||
  '<key>age</key><integer>30</integer>' || chr(10) ||
  '</dict>' || chr(10) || '</plist>', 'age') AS age;
SELECT plist_get(
  '<?xml version="1.0" encoding="UTF-8"?>' || chr(10) ||
  '<plist version="1.0">' || chr(10) || '<dict>' || chr(10) ||
  '<key>name</key><string>Alice</string>' || chr(10) ||
  '<key>age</key><integer>30</integer>' || chr(10) ||
  '</dict>' || chr(10) || '</plist>', 'missing') AS missing;
SELECT plist_to_json(
  '<?xml version="1.0" encoding="UTF-8"?>' || chr(10) ||
  '<plist version="1.0">' || chr(10) || '<dict>' || chr(10) ||
  '<key>name</key><string>Alice</string>' || chr(10) ||
  '<key>age</key><integer>30</integer>' || chr(10) ||
  '</dict>' || chr(10) || '</plist>') AS json;
SELECT plist_to_json('not a plist') AS bad;
