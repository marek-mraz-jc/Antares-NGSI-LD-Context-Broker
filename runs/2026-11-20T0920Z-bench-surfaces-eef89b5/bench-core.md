perf gate (bench, Intel(R) Xeon(R) 6975P-C, threshold 15%)

| row | baseline | this run | change | verdict |
|---|---|---|---|---|
| jsonld_compact_page_100 | none | 99.4534 | | not gated |
| ql_check_type_selection | none | 0.046 | | not gated |
| ql_geo_from_params | none | 0.2109 | | not gated |
| ql_parse_q | none | 0.5579 | | not gated |
| sql_compile_geo_near | none | 4.6142 | | not gated |
| sql_compile_geo_within | none | 11.0013 | | not gated |
| sql_compile_q | none | 4.2287 | | not gated |
| sql_compile_scope_q | none | 0.3974 | | not gated |

no baseline for Intel(R) Xeon(R) 6975P-C: not gated; commit it with --write
