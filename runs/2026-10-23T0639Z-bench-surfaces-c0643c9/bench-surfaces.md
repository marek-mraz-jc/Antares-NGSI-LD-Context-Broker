| budget row | bench | number | verdict |
|---|---|---|---|
| NGSI-LD view: p50 ≤ 1.05×, p99 ≤ 1.10× of the root | `antares-ops --bench view_query` | view / root: p50 1.09× (≤ 1.05×), p99 1.07× (≤ 1.10×) | over |
| view load and composition (no row; reported) | `antares-ops --bench views` | compose a view with an engine's rules: 45733 ns/op (2000 runs);query through the largest view, narrowing engine: 193 ns/op (2000 runs);query through a one-type view, narrowing engine: 162 ns/op (20000 runs) | reported |
| OData read: p50 ≤ vanilla + 0.15 ms + 1 µs/Entity, p99 ≤ 1.25× | `antares-odata --bench render` | budget OData read (100 Entities): p50 516 µs of 555 µs within, p99 1293 µs of 452 µs over | over |
| MCP tool call: ≤ vanilla + 0.2 ms + 1 µs/Entity | `antares-mcp --bench call` | budget MCP tool call (20 Entities): mean 379 µs of 276 µs over | over |
| $filter parse (no row; reported) | `odata-url --bench parse` | parse/filter: 2.484µs per parse, 112 bytes;parse/lambda: 2.132µs per parse, 120 bytes | reported |
| $metadata build (LinkML document row is a 100-class schema; this fixture is reported) | `odata-csdl --bench metadata` | metadata/xml: 44.312µs per build, 10964 bytes;metadata/json: 46.916µs per build, 8048 bytes | reported |

Internal CSR forward (p99 ≤ 0.5× the HTTP forward) has no bench: no crate serves it yet.
Report-only: no noise profile on the perf-weekly instance type yet (docs/src/performance.md).
