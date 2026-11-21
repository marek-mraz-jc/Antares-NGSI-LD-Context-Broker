perf gate (bench, Intel(R) Xeon(R) 6975P-C, threshold 15%)

| row | baseline | this run | change | verdict |
|---|---|---|---|---|
| jsonld_compact_page_100 | none | 101.2181 | | not gated |
| ql_check_type_selection | none | 0.0456 | | not gated |
| ql_geo_from_params | none | 0.2091 | | not gated |
| ql_parse_q | none | 0.5677 | | not gated |
| sql_compile_geo_near | none | 4.626 | | not gated |
| sql_compile_geo_within | none | 10.8325 | | not gated |
| sql_compile_q | none | 4.2136 | | not gated |
| sql_compile_scope_q | none | 0.4123 | | not gated |

no baseline for Intel(R) Xeon(R) 6975P-C: not gated; commit it with --write
