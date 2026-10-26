perf gate (bench, AMD EPYC 7763 64-Core Processor, threshold 15%)

| row | baseline | this run | change | verdict |
|---|---|---|---|---|
| jsonld_compact_page_100 | none | 134.8848 | | not gated |
| ql_check_type_selection | none | 0.0688 | | not gated |
| ql_geo_from_params | none | 0.2833 | | not gated |
| ql_parse_q | none | 0.7784 | | not gated |
| sql_compile_geo_near | none | 1.1359 | | not gated |
| sql_compile_geo_within | none | 1.8652 | | not gated |
| sql_compile_q | none | 4.9318 | | not gated |
| sql_compile_scope_q | none | 0.5082 | | not gated |

no baseline for AMD EPYC 7763 64-Core Processor: not gated; commit it with --write
