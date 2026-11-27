perf gate (bench, Intel(R) Xeon(R) 6975P-C, threshold 15%)

| row | baseline | this run | change | verdict |
|---|---|---|---|---|
| jsonld_compact_page_100 | none | 102.4467 | | not gated |
| ql_check_type_selection | none | 0.051 | | not gated |
| ql_geo_from_params | none | 0.2047 | | not gated |
| ql_parse_q | none | 0.5768 | | not gated |
| sql_compile_geo_near | none | 4.7106 | | not gated |
| sql_compile_geo_within | none | 11.1393 | | not gated |
| sql_compile_q | none | 4.2791 | | not gated |
| sql_compile_scope_q | none | 0.4238 | | not gated |

no baseline for Intel(R) Xeon(R) 6975P-C: not gated; commit it with --write
