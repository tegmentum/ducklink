-- icebergscan smoke: inspect an Apache Iceberg table's top metadata.json.
-- Fixture: a tiny Iceberg v2 metadata.json (format-version 2) with one schema
-- (2 fields: id long required, data string optional) and one snapshot. Passed
-- as a single VARCHAR literal so no CR/LF handling is needed.
--
-- iceberg_schema must emit the 2 fields; iceberg_snapshots must emit the 1 snapshot.
SELECT field_id, name, type, required
FROM iceberg_schema('{"format-version":2,"table-uuid":"9c12d441-03fe-4693-9a96-a0705ddf69c1","location":"s3://bucket/db/t","last-updated-ms":1602638573874,"current-schema-id":0,"schemas":[{"type":"struct","schema-id":0,"fields":[{"id":1,"name":"id","required":true,"type":"long"},{"id":2,"name":"data","required":false,"type":"string"}]}],"current-snapshot-id":3055729675574597004,"snapshots":[{"snapshot-id":3055729675574597004,"sequence-number":1,"timestamp-ms":1602638573874,"manifest-list":"s3://bucket/db/t/metadata/snap-1.avro"}]}')
ORDER BY field_id;

-- The single snapshot.
SELECT snapshot_id, sequence_number, timestamp_ms, manifest_list
FROM iceberg_snapshots('{"snapshots":[{"snapshot-id":3055729675574597004,"sequence-number":1,"timestamp-ms":1602638573874,"manifest-list":"s3://bucket/db/t/metadata/snap-1.avro"}]}')
ORDER BY snapshot_id;

-- Top-level metadata scalars: spot-check format-version.
SELECT value AS format_version
FROM iceberg_metadata('{"format-version":2,"table-uuid":"abc"}')
WHERE key = 'format-version';

-- Invalid JSON -> zero rows (proves the function is wired, never panics).
SELECT count(*) AS bad_rows FROM iceberg_schema('not json');

-- NULL input -> zero rows.
SELECT count(*) AS null_rows FROM iceberg_snapshots(NULL);
