perf gate (bench, Intel(R) Xeon(R) Platinum 8370C CPU @ 2.80GHz, threshold 15%)

| row | baseline | this run | change | verdict |
|---|---|---|---|---|
| jsonld_compact_page_100 | none | 111.0048 | | not gated |
| ql_check_type_selection | none | 0.05 | | not gated |
| ql_geo_from_params | none | 0.242 | | not gated |
| ql_parse_q | none | 0.7074 | | not gated |
| sql_compile_geo_near | none | 5.2782 | | not gated |
| sql_compile_geo_within | none | 12.3686 | | not gated |
| sql_compile_q | none | 4.7511 | | not gated |
| sql_compile_scope_q | none | 0.4894 | | not gated |

no baseline for Intel(R) Xeon(R) Platinum 8370C CPU @ 2.80GHz: not gated; commit it with --write
