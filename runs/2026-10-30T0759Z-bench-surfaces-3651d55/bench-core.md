perf gate (bench, Intel(R) Xeon(R) Platinum 8370C CPU @ 2.80GHz, threshold 15%)

| row | baseline | this run | change | verdict |
|---|---|---|---|---|
| jsonld_compact_page_100 | none | 111.9676 | | not gated |
| ql_check_type_selection | none | 0.0526 | | not gated |
| ql_geo_from_params | none | 0.2387 | | not gated |
| ql_parse_q | none | 0.6959 | | not gated |
| sql_compile_geo_near | none | 5.2812 | | not gated |
| sql_compile_geo_within | none | 12.3099 | | not gated |
| sql_compile_q | none | 4.2148 | | not gated |
| sql_compile_scope_q | none | 0.513 | | not gated |

no baseline for Intel(R) Xeon(R) Platinum 8370C CPU @ 2.80GHz: not gated; commit it with --write
