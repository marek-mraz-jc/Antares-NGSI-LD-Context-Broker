perf gate (bench, Intel(R) Xeon(R) 6975P-C, threshold 15%)

| row | baseline | this run | change | verdict |
|---|---|---|---|---|
| jsonld_compact_page_100 | none | 97.3827 | | not gated |
| ql_check_type_selection | none | 0.065 | | not gated |
| ql_geo_from_params | none | 0.2118 | | not gated |
| ql_parse_q | none | 0.6264 | | not gated |
| sql_compile_geo_near | none | 4.7271 | | not gated |
| sql_compile_geo_within | none | 11.23 | | not gated |
| sql_compile_q | none | 4.243 | | not gated |
| sql_compile_scope_q | none | 0.4122 | | not gated |

no baseline for Intel(R) Xeon(R) 6975P-C: not gated; commit it with --write
