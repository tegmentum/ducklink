-- mime extension smoke.
SELECT mime_type('photo.png') AS png;
SELECT mime_type('/var/data/report.pdf') AS pdf;
SELECT mime_from_ext('json') AS json;
SELECT mime_type('noextension') AS bad;
