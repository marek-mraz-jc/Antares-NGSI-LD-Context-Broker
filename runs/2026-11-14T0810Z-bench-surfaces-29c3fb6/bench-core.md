perf gate (bench, AMD EPYC 9V74 80-Core Processor, threshold 15%)

| row | baseline | this run | change | verdict |
|---|---|---|---|---|
| jsonld_compact_page_100 | none | 112.7887 | | not gated |
| ql_check_type_selection | none | 0.0491 | | not gated |
| ql_geo_from_params | none | 0.2517 | | not gated |
| ql_parse_q | none | 0.6805 | | not gated |
| sql_compile_geo_near | none | 5.4518 | | not gated |
| sql_compile_geo_within | none | 13.2088 | | not gated |
| sql_compile_q | none | 4.0951 | | not gated |
| sql_compile_scope_q | none | 0.4027 | | not gated |

no baseline for AMD EPYC 9V74 80-Core Processor: not gated; commit it with --write
