-- tsid extension smoke (all deterministic: no clock/RNG).
-- Round-trip a known timestamp through the full TSID surface.
SELECT tsid_from_timestamp(1700000000000) AS id;
SELECT tsid_timestamp(tsid_from_timestamp(1700000000000)) AS ts;
SELECT tsid_encode(tsid_from_timestamp(1700000000000)) AS enc;
SELECT tsid_decode(tsid_encode(tsid_from_timestamp(1700000000000))) AS dec;
SELECT tsid_decode('not!valid') AS bad;
