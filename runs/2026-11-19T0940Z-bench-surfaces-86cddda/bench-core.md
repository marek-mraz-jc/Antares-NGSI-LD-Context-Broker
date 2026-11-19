perf gate (bench, AMD EPYC 9V74 80-Core Processor, threshold 15%)

| row | baseline | this run | change | verdict |
|---|---|---|---|---|
| jsonld_compact_page_100 | none | 110.0 | | not gated |
| ql_check_type_selection | none | 0.0615 | | not gated |
| ql_geo_from_params | none | 0.2461 | | not gated |
| ql_parse_q | none | 0.6283 | | not gated |
| sql_compile_geo_near | none | 5.5071 | | not gated |
| sql_compile_geo_within | none | 13.1168 | | not gated |
| sql_compile_q | none | 4.3841 | | not gated |
| sql_compile_scope_q | none | 0.4257 | | not gated |

no baseline for AMD EPYC 9V74 80-Core Processor: not gated; commit it with --write
