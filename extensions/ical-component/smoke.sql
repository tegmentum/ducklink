-- ical extension smoke. The REPL collapses literal newlines inside string
-- literals, so the VCALENDAR is assembled with chr(10) line joins.
-- A tiny VCALENDAR with two VEVENTs.
SELECT ical_event_count(
  'BEGIN:VCALENDAR' || chr(10) ||
  'VERSION:2.0' || chr(10) ||
  'BEGIN:VEVENT' || chr(10) ||
  'UID:a@x' || chr(10) ||
  'SUMMARY:Hello' || chr(10) ||
  'DTSTART:20260101T100000Z' || chr(10) ||
  'DTEND:20260101T110000Z' || chr(10) ||
  'END:VEVENT' || chr(10) ||
  'BEGIN:VEVENT' || chr(10) ||
  'UID:b@x' || chr(10) ||
  'SUMMARY:World' || chr(10) ||
  'DTSTART:20260102T100000Z' || chr(10) ||
  'END:VEVENT' || chr(10) ||
  'END:VCALENDAR'
) AS cnt;
SELECT ical_summaries(
  'BEGIN:VCALENDAR' || chr(10) ||
  'VERSION:2.0' || chr(10) ||
  'BEGIN:VEVENT' || chr(10) ||
  'UID:a@x' || chr(10) ||
  'SUMMARY:Hello' || chr(10) ||
  'DTSTART:20260101T100000Z' || chr(10) ||
  'DTEND:20260101T110000Z' || chr(10) ||
  'END:VEVENT' || chr(10) ||
  'BEGIN:VEVENT' || chr(10) ||
  'UID:b@x' || chr(10) ||
  'SUMMARY:World' || chr(10) ||
  'DTSTART:20260102T100000Z' || chr(10) ||
  'END:VEVENT' || chr(10) ||
  'END:VCALENDAR'
) AS sums;
SELECT ical_to_json(
  'BEGIN:VCALENDAR' || chr(10) ||
  'VERSION:2.0' || chr(10) ||
  'BEGIN:VEVENT' || chr(10) ||
  'UID:a@x' || chr(10) ||
  'SUMMARY:Hello' || chr(10) ||
  'DTSTART:20260101T100000Z' || chr(10) ||
  'DTEND:20260101T110000Z' || chr(10) ||
  'END:VEVENT' || chr(10) ||
  'END:VCALENDAR'
) AS j;
SELECT ical_event_count('not a calendar') AS bad;
