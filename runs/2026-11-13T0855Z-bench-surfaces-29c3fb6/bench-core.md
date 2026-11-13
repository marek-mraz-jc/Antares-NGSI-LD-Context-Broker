perf gate (bench, Intel(R) Xeon(R) 6975P-C, threshold 15%)

| row | baseline | this run | change | verdict |
|---|---|---|---|---|
| jsonld_compact_page_100 | none | 99.2946 | | not gated |
| ql_check_type_selection | none | 0.0483 | | not gated |
| ql_geo_from_params | none | 0.2183 | | not gated |
| ql_parse_q | none | 0.6561 | | not gated |
| sql_compile_geo_near | none | 4.6747 | | not gated |
| sql_compile_geo_within | none | 11.288 | | not gated |
| sql_compile_q | none | 3.6893 | | not gated |
| sql_compile_scope_q | none | 0.4116 | | not gated |

no baseline for Intel(R) Xeon(R) 6975P-C: not gated; commit it with --write
