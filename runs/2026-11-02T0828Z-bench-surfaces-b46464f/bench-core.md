perf gate (bench, AMD EPYC 9V74 80-Core Processor, threshold 15%)

| row | baseline | this run | change | verdict |
|---|---|---|---|---|
| jsonld_compact_page_100 | none | 117.1667 | | not gated |
| ql_check_type_selection | none | 0.0614 | | not gated |
| ql_geo_from_params | none | 0.2539 | | not gated |
| ql_parse_q | none | 0.6879 | | not gated |
| sql_compile_geo_near | none | 5.8181 | | not gated |
| sql_compile_geo_within | none | 13.928 | | not gated |
| sql_compile_q | none | 3.9397 | | not gated |
| sql_compile_scope_q | none | 0.413 | | not gated |

no baseline for AMD EPYC 9V74 80-Core Processor: not gated; commit it with --write
