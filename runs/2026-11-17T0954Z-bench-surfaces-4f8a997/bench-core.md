perf gate (bench, Intel(R) Xeon(R) 6975P-C, threshold 15%)

| row | baseline | this run | change | verdict |
|---|---|---|---|---|
| jsonld_compact_page_100 | none | 96.8312 | | not gated |
| ql_check_type_selection | none | 0.0669 | | not gated |
| ql_geo_from_params | none | 0.2125 | | not gated |
| ql_parse_q | none | 0.6293 | | not gated |
| sql_compile_geo_near | none | 4.7106 | | not gated |
| sql_compile_geo_within | none | 11.2754 | | not gated |
| sql_compile_q | none | 4.1911 | | not gated |
| sql_compile_scope_q | none | 0.4306 | | not gated |

no baseline for Intel(R) Xeon(R) 6975P-C: not gated; commit it with --write
