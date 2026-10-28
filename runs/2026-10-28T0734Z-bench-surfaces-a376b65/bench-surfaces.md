| budget row | bench | number | verdict |
|---|---|---|---|
| NGSI-LD view: p50 ≤ 1.05×, p99 ≤ 1.10× of the root | `antares-ops --bench view_query` | view / root: p50 1.06× (≤ 1.05×), p99 1.07× (≤ 1.10×) | over |
| view load and composition (no row; reported) | `antares-ops --bench views` | compose a view with an engine's rules: 80314 ns/op (2000 runs);query through the largest view, narrowing engine: 366 ns/op (2000 runs);query through a one-type view, narrowing engine: 314 ns/op (20000 runs) | reported |
| OData read: p50 ≤ vanilla + 0.15 ms + 1 µs/Entity, p99 ≤ 1.25× | `antares-odata --bench render` | budget OData read (100 Entities): p50 1015 µs of 851 µs over, p99 1091 µs of 819 µs over | over |
| MCP tool call: ≤ vanilla + 0.2 ms + 1 µs/Entity | `antares-mcp --bench call` | budget MCP tool call (20 Entities): mean 866 µs of 335 µs over | over |
| $filter parse (no row; reported) | `odata-url --bench parse` | parse/filter: 4.913µs per parse, 112 bytes;parse/lambda: 3.993µs per parse, 120 bytes;parse/expand: 1.729µs per parse, 101 bytes | reported |
| $metadata build (LinkML document row is a 100-class schema; this fixture is reported) | `odata-csdl --bench metadata` | metadata/xml: 82.109µs per build, 10964 bytes;metadata/json: 92.337µs per build, 8048 bytes | reported |

Internal CSR forward (p99 ≤ 0.5× the HTTP forward) has no bench: no crate serves it yet.
Report-only: no noise profile on the perf-weekly instance type yet (docs/src/performance.md).
