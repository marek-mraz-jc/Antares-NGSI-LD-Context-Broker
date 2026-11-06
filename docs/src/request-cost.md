# Request cost

With `ANTARES_REQUEST_COST=on` every answer of the broker says what the
request cost in compute, and `/q/metrics` keeps the totals per Tenant and
surface, so a platform in front of the broker can split the cost of one
broker between organizations. The broker reports raw measures only; it
prices nothing. The switch is `off` by default, and off measures nothing:
no layer is built, no clock is read and no header is added. Any other
value than `on` or `off` stops the broker at boot.

## Headers

```text
Server-Timing: app;dur=1.2, db;dur=0.8, pool;dur=0.1, total;dur=2.4
JC-Request-Cost: cpu-ms=1.234, db-ms=0.812, statements=3, rows=20
```

`Server-Timing` is the [W3C Server Timing](https://www.w3.org/TR/server-timing/)
header that browser developer tools and APM agents read; its durations
are milliseconds with one decimal. `JC-Request-Cost` is an
[RFC 8941](https://www.rfc-editor.org/rfc/rfc8941) dictionary carrying the
counts Server-Timing has no place for, with durations to three decimals.

| measure | header member | what it counts |
|---|---|---|
| app | `app`, `cpu-ms` | time spent inside the request's `poll` calls: the request's busy time on the runtime, summed over every poll, including the work of an in-process call on a task of its own |
| db | `db`, `db-ms` | wall time of every store statement the request awaited, from sqlx's own per-statement event |
| pool | `pool` | time spent waiting for a pool connection, with the `BEGIN` that hands it over |
| statements | `statements` | store statements run |
| rows | `rows` | rows the store returned; only rows row-level security let this request see |
| total | `total` | wall time from the request entering the router to the response headers |

Every surface (NGSI-LD, OData, OGC, STA, MCP over HTTP) is covered by one
layer over the router. A surface that answers through an in-process
NGSI-LD call adds that call's cost to its own request: the client sees one
pair of headers with the summed cost. A WebSocket upgrade carries the
headers on its `101` answer; the frames after it are not counted.

Not part of a request's cost: a streamed body's bytes (the headers leave
before the body; the bytes go only into the counter below), work on
`spawn_blocking` threads, and background tasks the request starts or that
run on their own clock (notifications, the outbox drain, maintenance,
federation forwards on tasks of their own). `db`, `statements` and
`rows` count the PostgreSQL store; the memory and file stores answer
zero. `app` includes the time the request's code spends on CPU; it does
not read process CPU time, so a busy neighbour on the same thread shows
in `total`, not in `app`.

## Counters

With `ANTARES_TELEMETRY` on, `/q/metrics` carries counters labelled
`tenant` (the `NGSILD-Tenant` the caller sent, empty for the default
Tenant) and `surface` (`ngsi-ld`, `admin`, the surface below a view such
as `odata`, or `other`):

| counter | unit |
|---|---|
| `antares_request_cost_requests_total` | requests |
| `antares_request_cost_app_microseconds_total` | µs of app time |
| `antares_request_cost_db_microseconds_total` | µs of statement time |
| `antares_request_cost_pool_microseconds_total` | µs of pool wait |
| `antares_request_cost_statements_total` | statements |
| `antares_request_cost_rows_total` | rows |
| `antares_request_cost_response_bytes_total` | body bytes, before compression |

The durations count whole microseconds because the metrics recorder keeps
integer counters. The first 1000 Tenant names get a label of their own;
later ones share `_other`, so a caller sending made-up Tenant names cannot
grow the recorder without bound.

## Security

- The figures describe the caller's own request under its own row-level
  security scope. No Tenant name, SQL text, host name or internal id
  appears in a header; the per-tenant label is on `/q/metrics` only,
  which a deployment serves on its metrics listener (`ANTARES_METRICS_PORT`).
- Timings are a side channel: a caller comparing the cost of two requests
  can learn something about data it cannot read (how many rows a filter
  scanned before policy removed them, how loaded the database is with
  other tenants' work). Turn the switch on only where the gateway in front
  of the broker shows the headers to trusted callers, or strips them and
  keeps only its own totals per organization.

## Related

- [Configuration](configuration.md)
- [Operations](operations.md)
- [Performance](performance.md)
