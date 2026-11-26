perf gate (bench, Intel(R) Xeon(R) 6973P-C, threshold 15%)

| row | baseline | this run | change | verdict |
|---|---|---|---|---|
| jsonld_compact_page_100 | none | 105.3797 | | not gated |
| ql_check_type_selection | none | 0.0521 | | not gated |
| ql_geo_from_params | none | 0.204 | | not gated |
| ql_parse_q | none | 0.6186 | | not gated |
| sql_compile_geo_near | none | 4.7554 | | not gated |
| sql_compile_geo_within | none | 11.293 | | not gated |
| sql_compile_q | none | 4.324 | | not gated |
| sql_compile_scope_q | none | 0.3801 | | not gated |

no baseline for Intel(R) Xeon(R) 6973P-C: not gated; commit it with --write
