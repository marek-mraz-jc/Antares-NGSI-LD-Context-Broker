perf gate (bench, INTEL(R) XEON(R) PLATINUM 8573C, threshold 15%)

| row | baseline | this run | change | verdict |
|---|---|---|---|---|
| jsonld_compact_page_100 | none | 101.3756 | | not gated |
| ql_check_type_selection | none | 0.0709 | | not gated |
| ql_geo_from_params | none | 0.2259 | | not gated |
| ql_parse_q | none | 0.6213 | | not gated |
| sql_compile_geo_near | none | 5.0887 | | not gated |
| sql_compile_geo_within | none | 11.6547 | | not gated |
| sql_compile_q | none | 4.0303 | | not gated |
| sql_compile_scope_q | none | 0.4366 | | not gated |

no baseline for INTEL(R) XEON(R) PLATINUM 8573C: not gated; commit it with --write
