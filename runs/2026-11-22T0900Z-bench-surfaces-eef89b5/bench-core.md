perf gate (bench, AMD EPYC 9V74 80-Core Processor, threshold 15%)

| row | baseline | this run | change | verdict |
|---|---|---|---|---|
| jsonld_compact_page_100 | none | 110.4179 | | not gated |
| ql_check_type_selection | none | 0.0444 | | not gated |
| ql_geo_from_params | none | 0.2391 | | not gated |
| ql_parse_q | none | 0.6155 | | not gated |
| sql_compile_geo_near | none | 5.5459 | | not gated |
| sql_compile_geo_within | none | 13.3734 | | not gated |
| sql_compile_q | none | 4.3391 | | not gated |
| sql_compile_scope_q | none | 0.4246 | | not gated |

no baseline for AMD EPYC 9V74 80-Core Processor: not gated; commit it with --write
