perf gate (bench, AMD EPYC 7763 64-Core Processor, threshold 15%)

| row | baseline | this run | change | verdict |
|---|---|---|---|---|
| jsonld_compact_page_100 | none | 127.2686 | | not gated |
| ql_check_type_selection | none | 0.0529 | | not gated |
| ql_geo_from_params | none | 0.2675 | | not gated |
| ql_parse_q | none | 0.7533 | | not gated |
| sql_compile_geo_near | none | 6.1594 | | not gated |
| sql_compile_geo_within | none | 14.5262 | | not gated |
| sql_compile_q | none | 5.1987 | | not gated |
| sql_compile_scope_q | none | 0.5115 | | not gated |

no baseline for AMD EPYC 7763 64-Core Processor: not gated; commit it with --write
