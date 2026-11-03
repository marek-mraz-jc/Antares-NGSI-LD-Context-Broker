| budget row | bench | number | verdict |
|---|---|---|---|
| NGSI-LD view: p50 ≤ 1.05×, p99 ≤ 1.10× of the root | `antares-ops --bench view_query` | view / root: p50 1.11× (≤ 1.05×), p99 1.35× (≤ 1.10×) | over |
| view load and composition (no row; reported) | `antares-ops --bench views` | compose a view with an engine's rules: 42699 ns/op (2000 runs);query through the largest view, narrowing engine: 187 ns/op (2000 runs);query through a one-type view, narrowing engine: 161 ns/op (20000 runs) | reported |
| OData read: p50 ≤ vanilla + 0.15 ms + 1 µs/Entity, p99 ≤ 1.25× | `antares-odata --bench render` | budget OData read (100 Entities): p50 618 µs of 668 µs within, p99 703 µs of 574 µs over | over |
| MCP tool call: ≤ vanilla + 0.2 ms + 1 µs/Entity | `antares-mcp --bench call` | budget MCP tool call (20 Entities): mean 539 µs of 289 µs over | over |
| $filter parse (no row; reported) | `odata-url --bench parse` | parse/filter: 2.57µs per parse, 112 bytes;parse/lambda: 2.294µs per parse, 120 bytes | reported |
| $metadata build (LinkML document row is a 100-class schema; this fixture is reported) | `odata-csdl --bench metadata` | metadata/xml: 43.89µs per build, 10964 bytes;metadata/json: 48.686µs per build, 8048 bytes | reported |

Internal CSR forward (p99 ≤ 0.5× the HTTP forward) has no bench: no crate serves it yet.
Report-only: no noise profile on the perf-weekly instance type yet (docs/src/performance.md).
