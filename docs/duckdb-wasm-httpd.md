# duckdb-wasm-httpd (`ducklink serve`)

An HTTP/HTTPS server that executes SQL against the wasm DuckDB **core
component** and returns JSON. A port of
[`sqlite-wasm-httpd`](../../sqlite-wasm/sqlite-wasm-httpd) onto DuckDB's wasm
core — same contract (built-in admin endpoints + a database-driven `routes`
table + TLS), adapted to the DuckDB substrate.

The native host owns the listening socket and runs every query through the
core component over the `database` WIT interface (there is no native libduckdb
in the hot path). It's **single-threaded** by design — one core instance + one
connection held in the accept loop, one request per connection
(`Connection: close`), like the UI server (`ducklink ui`).

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
| GET | `/tables` | JSON list of user-table names (`information_schema.tables`) |
| GET | `/schema/{name}` | `pragma_table_info` JSON |

These take precedence over any db-driven route.

## Database-driven router

A route is a row mapping `(method, GLOB pattern)` to a handler:

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

`handler` is SQL run against the connection. The request fields are bound as
DuckDB **named parameters** — reference any subset, in any order:

| param | value |
|---|---|
| `$method` | request method |
| `$path` | request path |
| `$query` | raw query string (NULL if none) |
| `$body` | request body as text (NULL if none) |
| `$remote` | peer address |

(The sqlite original uses `:name`; DuckDB's WIT `execute` is positional, so the
server resolves these named tokens to values in first-appearance order — the
order DuckDB numbers them. A token of one of these names inside a string
literal or `$tag$` dollar-quote would be miscounted; operator SQL rarely does
that.)

Result interpretation (identical to the sqlite port):
- **0 rows** → 204 No Content
- **1 row, 1 column** → that value IS the response body
- **1 row with `body`/`status`/`ctype` columns** → structured response
- **1 row, multiple columns** → JSON object of the row
- **>1 rows** → JSON array of row-objects

### kind = 'static'

`handler` IS the response body verbatim. No SQL roundtrip. `status`/`ctype`
columns still apply.

### kind = 'blob'

`handler` is SQL returning ONE value (first column of first row), emitted as
the raw response body — for binary serving (default ctype
`application/octet-stream`). Zero rows → 404.

### kind = 'wasm'

`handler` names a wasm component pre-loaded via `--load NAME=PATH` at startup.
The component implements the `duckdb:handler/request-handler` world — it exports
`handler.handle(request: string) -> result<string, string>`. The request is
serialized to JSON:

```
{
  "method":  "POST",
  "path":    "/upload",
  "query":   "v=1" | null,
  "remote":  "127.0.0.1:55432",
  "headers": { "content-type": "application/json", ... },
  "body":    { "text": "..." } | { "bytes_hex": "deadbeef..." }
}
```

The component returns the response body, or a JSON object
`{ "status": 201, "body": "...", "ctype": "text/plain" }` to override the
status/content-type; an `Err` becomes a 500. Each request gets a **fresh
wasmtime Store** (stateless across requests — persistent state belongs in the
DB). A route naming an unloaded handler returns 500; if no handlers were loaded
at all, `kind='wasm'` returns 501.

```
ducklink serve --load upper=upper_handler.wasm --env JWT_SECRET
```

`--env KEY[=VALUE]` forwards env into every handler (no process env is exposed
otherwise). The reference handler is `handlers/echo-handler` (build with
`cargo component build -p echo-handler --target wasm32-wasip2 --release`).

## Quickstart

```
$ ducklink serve --init-routes
duckdb-httpd: routes table `routes` ready
duckdb-httpd: http://127.0.0.1:8080  db=:memory:  POST /sql | GET /sql?q=...

$ curl http://localhost:8080/hello
{}

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

Blocking rustls over the accept loop's TcpStream; `rcgen` mints the
self-signed cert. (No client-cert auth in v1.)

## Smoke test

`test/smoke-httpd.sh` exercises the built-ins + every route kind end-to-end.
