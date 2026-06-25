---
id: serve
title: The HTTP server (ducklink serve)
sidebar_label: HTTP server
---

# `ducklink serve` — SQL over HTTP

`ducklink serve` is an HTTP/HTTPS server that executes SQL against the wasm DuckDB
**core component** and returns JSON. It is a port of `sqlite-wasm-httpd` onto
DuckDB's wasm core — the same contract (built-in admin endpoints + a
database-driven `routes` table + TLS), adapted to the DuckDB substrate.

The native host owns the listening socket and runs every query through the core
component over the `database` WIT interface (no native libduckdb in the hot path).
It is **single-threaded** by design — one core instance + one connection held in
the accept loop, one request per connection (`Connection: close`).

```
ducklink serve [--db PATH] [--bind ADDR] [--port N]
               [--routes-table T] [--init-routes]
               [--tls-self-signed | --tls-cert C --tls-key K]
```

| flag | default | meaning |
|---|---|---|
| `--db PATH` | `:memory:` | database path (`:memory:` keeps it in process) |
| `--bind ADDR` | `127.0.0.1` | bind address |
| `--port N` | `8080` | TCP port |
| `--routes-table T` | `routes` | table consulted for db-driven routing |
| `--init-routes` | off | create + seed the routes table (idempotent) |
| `--tls-self-signed` | off | HTTPS with a generated self-signed cert (dev/smoke) |
| `--tls-cert C --tls-key K` | off | HTTPS with operator-supplied PEMs |

## Built-in endpoints

| Method | Path | Behaviour |
|---|---|---|
| GET | `/health` | 200 `ok` |
| POST | `/sql` | body is the SQL; returns `{columns, rows, rowcount}` JSON |
| GET | `/sql?q=URL_ENCODED` | same, GET form |
| GET | `/tables` | JSON list of user-table names |
| GET | `/schema/{name}` | `pragma_table_info` JSON |

These take precedence over any db-driven route.

## Database-driven router

A route maps `(method, GLOB pattern)` to a handler:

```sql
CREATE TABLE routes (
    method   VARCHAR NOT NULL,         -- 'GET', 'POST', or '*'
    pattern  VARCHAR NOT NULL,         -- GLOB pattern: '/users/*', '/health'
    handler  VARCHAR NOT NULL,         -- depends on kind
    kind     VARCHAR NOT NULL DEFAULT 'sql',  -- 'sql' | 'static' | 'blob' | 'wasm'
    status   INTEGER DEFAULT 200,
    ctype    VARCHAR,                  -- default application/json
    priority INTEGER DEFAULT 0         -- higher matches first
);
```

Best match: `(method = ? OR method = '*') AND ? GLOB pattern`, ordered by
`priority DESC, length(pattern) DESC`.

### kind = 'sql'

`handler` is SQL run against the connection. Request fields are bound as DuckDB
**named parameters** — `$method`, `$path`, `$query`, `$body`, `$remote`.

Result interpretation: 0 rows → 204; 1 row × 1 column → that value is the body;
1 row with `body`/`status`/`ctype` columns → structured response; 1 row ×
multiple columns → JSON object; more than one row → JSON array of row-objects.

### kind = 'static'

`handler` is the response body verbatim. No SQL roundtrip.

### kind = 'blob'

`handler` is SQL returning ONE value, emitted as the raw response body — for
binary serving (default ctype `application/octet-stream`). Zero rows → 404.

### kind = 'wasm'

`handler` names a wasm component pre-loaded via `--load NAME=PATH` at startup,
implementing the `duckdb:handler/request-handler` world — it exports
`handler.handle(request: string) -> result<string, string>`. The request is
serialized to JSON:

```json
{
  "method":  "POST",
  "path":    "/upload",
  "query":   "v=1",
  "remote":  "127.0.0.1:55432",
  "headers": { "content-type": "application/json" },
  "body":    { "text": "..." }
}
```

The component returns the response body, or a JSON object
`{ "status": 201, "body": "...", "ctype": "text/plain" }` to override. Each
request gets a fresh wasmtime Store (stateless — persistent state belongs in the
DB). `--env KEY[=VALUE]` forwards env into every handler. The reference handler is
`handlers/echo-handler`.

## Quickstart

```
$ ducklink serve --init-routes
duckdb-httpd: routes table `routes` ready
duckdb-httpd: http://127.0.0.1:8080  db=:memory:  POST /sql | GET /sql?q=...

$ curl -X POST http://localhost:8080/sql -d \
    "INSERT INTO routes (method, pattern, handler, ctype) VALUES \
     ('POST', '/upper', 'SELECT upper(\$body) AS body', 'text/plain')"

$ curl -X POST http://localhost:8080/upper -d 'hello world'
HELLO WORLD
```

## TLS

```
ducklink serve                                   # plain HTTP
ducklink serve --tls-self-signed                 # HTTPS, generated cert
ducklink serve --tls-cert server.crt --tls-key server.key
```

Blocking rustls over the accept loop's TcpStream; `rcgen` mints the self-signed
cert. `test/smoke-httpd.sh` exercises the built-ins + every route kind
end-to-end.
