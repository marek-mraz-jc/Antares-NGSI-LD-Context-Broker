perf gate (bench, AMD EPYC 9V74 80-Core Processor, threshold 15%)

| row | baseline | this run | change | verdict |
|---|---|---|---|---|
| jsonld_compact_page_100 | none | 113.8856 | | not gated |
| ql_check_type_selection | none | 0.0497 | | not gated |
| ql_geo_from_params | none | 0.2548 | | not gated |
| ql_parse_q | none | 0.6843 | | not gated |
| sql_compile_geo_near | none | 0.9963 | | not gated |
| sql_compile_geo_within | none | 1.6047 | | not gated |
| sql_compile_q | none | 3.8602 | | not gated |
| sql_compile_scope_q | none | 0.4319 | | not gated |

no baseline for AMD EPYC 9V74 80-Core Processor: not gated; commit it with --write
