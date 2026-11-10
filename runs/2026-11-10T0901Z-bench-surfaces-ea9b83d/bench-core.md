perf gate (bench, AMD EPYC 9V45 96-Core Processor, threshold 15%)

| row | baseline | this run | change | verdict |
|---|---|---|---|---|
| jsonld_compact_page_100 | none | 91.8573 | | not gated |
| ql_check_type_selection | none | 0.0366 | | not gated |
| ql_geo_from_params | none | 0.2166 | | not gated |
| ql_parse_q | none | 0.5105 | | not gated |
| sql_compile_geo_near | none | 5.1259 | | not gated |
| sql_compile_geo_within | none | 12.0972 | | not gated |
| sql_compile_q | none | 3.7926 | | not gated |
| sql_compile_scope_q | none | 0.3505 | | not gated |

no baseline for AMD EPYC 9V45 96-Core Processor: not gated; commit it with --write
