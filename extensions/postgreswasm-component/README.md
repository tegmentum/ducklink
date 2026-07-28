# postgreswasm-component

PostgreSQL storage backend for DuckLink, delivered as a wasm component that
speaks the PostgreSQL v3 wire protocol over `wasi:sockets`. Backs
`ATTACH '<dsn>' (TYPE postgreswasm)` and dispatches scans + writes through the
`duckdb:extension/storage-dispatch` + `.../storage-write-dispatch` WIT
interfaces (ADR Amendments A1 + A5).

DSNs accepted:

- URL form: `postgres://user:pw@host:port/db`
- Key/value form: `host=.. port=.. user=.. password=.. database=..`

The component reads/writes plaintext only (no TLS) and is intended for a
localhost / trusted-VLAN server. Network access requires the host's
`DUCKLINK_NETWORK_GRANT` capability.

## Known limitations

- **Auth**: Only `trust` and `md5` are supported. Postgres 16+ defaults to
  `scram-sha-256`, which this client does not implement — the connection
  attempt returns `SCRAM auth not supported; use trust/md5`. To connect,
  either set the server-side auth method explicitly (e.g.
  `POSTGRES_HOST_AUTH_METHOD=md5` and `POSTGRES_INITDB_ARGS="--auth-host=md5"`
  on the `postgres:16` Docker image), or add an md5/trust `pg_hba.conf` entry
  for the user. SCRAM support is tracked as a future enhancement (Bug 5c).
- **No TLS**: connections are plaintext. Do not use over untrusted networks.
- **rowid semantics**: UPDATE / DELETE pack Postgres's per-tuple `ctid`
  `(block, offset)` into an int64 as the advertised `rowid`. A concurrent
  `VACUUM FULL` / `CLUSTER` between the write pre-scan and dispatch can
  invalidate the packed rowid; the heap access method is the only supported
  storage backend.
