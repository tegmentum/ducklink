-- ids extension smoke (generators are nondeterministic; assert shape, not value).
-- ulid() is 26 chars, nanoid() is 21 chars.
SELECT length(ulid()) AS ulid_len;
SELECT length(nanoid()) AS nanoid_len;
SELECT ulid_timestamp('01ARZ3NDEKTSV4RRFFQ69G5FAV') AS ts;
SELECT ulid_timestamp('not-a-ulid') AS bad;
