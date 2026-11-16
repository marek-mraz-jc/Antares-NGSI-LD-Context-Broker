perf gate (bench, Intel(R) Xeon(R) 6975P-C, threshold 15%)

| row | baseline | this run | change | verdict |
|---|---|---|---|---|
| jsonld_compact_page_100 | none | 98.3206 | | not gated |
| ql_check_type_selection | none | 0.0664 | | not gated |
| ql_geo_from_params | none | 0.2117 | | not gated |
| ql_parse_q | none | 0.6298 | | not gated |
| sql_compile_geo_near | none | 5.0717 | | not gated |
| sql_compile_geo_within | none | 11.1506 | | not gated |
| sql_compile_q | none | 4.2319 | | not gated |
| sql_compile_scope_q | none | 0.4096 | | not gated |

no baseline for Intel(R) Xeon(R) 6975P-C: not gated; commit it with --write
