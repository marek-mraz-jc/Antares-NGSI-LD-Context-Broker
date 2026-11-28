perf gate (bench, INTEL(R) XEON(R) PLATINUM 8573C, threshold 15%)

| row | baseline | this run | change | verdict |
|---|---|---|---|---|
| jsonld_compact_page_100 | none | 104.1773 | | not gated |
| ql_check_type_selection | none | 0.052 | | not gated |
| ql_geo_from_params | none | 0.2092 | | not gated |
| ql_parse_q | none | 0.578 | | not gated |
| sql_compile_geo_near | none | 4.9292 | | not gated |
| sql_compile_geo_within | none | 11.6735 | | not gated |
| sql_compile_q | none | 4.4852 | | not gated |
| sql_compile_scope_q | none | 0.445 | | not gated |

no baseline for INTEL(R) XEON(R) PLATINUM 8573C: not gated; commit it with --write
