| budget row | bench | number | verdict |
|---|---|---|---|
| NGSI-LD view: p50 ≤ 1.05×, p99 ≤ 1.10× of the root | `antares-ops --bench view_query` | view / root: p50 1.06× (≤ 1.05×), p99 1.04× (≤ 1.10×) | over |
| view load and composition (no row; reported) | `antares-ops --bench views` | compose a view with an engine's rules: 79865 ns/op (2000 runs);query through the largest view, narrowing engine: 341 ns/op (2000 runs);query through a one-type view, narrowing engine: 304 ns/op (20000 runs) | reported |
| OData read: p50 ≤ vanilla + 0.15 ms + 1 µs/Entity, p99 ≤ 1.25× | `antares-odata --bench render` | budget OData read (100 Entities): p50 1190 µs of 1053 µs over, p99 1290 µs of 1940 µs within | over |
| MCP tool call: ≤ vanilla + 0.2 ms + 1 µs/Entity | `antares-mcp --bench call` | budget MCP tool call (20 Entities): mean 1056 µs of 354 µs over | over |
| $filter parse (no row; reported) | `odata-url --bench parse` | parse/filter: 4.99µs per parse, 112 bytes;parse/lambda: 4.316µs per parse, 120 bytes;parse/expand: 1.985µs per parse, 101 bytes | reported |
| $metadata build (LinkML document row is a 100-class schema; this fixture is reported) | `odata-csdl --bench metadata` | metadata/xml: 82.945µs per build, 10964 bytes;metadata/json: 91.963µs per build, 8048 bytes | reported |
| WebSocket: socket p99 ≤ HTTP p99, ≤ 32 KiB per idle connection | `antares-ws --bench fanout` | latency over 2000 changes: socket p50 1.898567ms p99 3.085953ms; HTTP p50 1.849689ms p99 3.037117ms; socket p99 within 1.10× HTTP p99: true;resident per idle connection: 21 KiB over 2000 sockets (budget 32 KiB): within | over |

Internal CSR forward (p99 ≤ 0.5× the HTTP forward) has no bench: no crate serves it yet.
Report-only: no noise profile on the perf-weekly instance type yet (docs/src/performance.md).
