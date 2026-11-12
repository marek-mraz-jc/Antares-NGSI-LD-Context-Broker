perf gate (bench, AMD EPYC 7763 64-Core Processor, threshold 15%)

| row | baseline | this run | change | verdict |
|---|---|---|---|---|
| jsonld_compact_page_100 | none | 134.0289 | | not gated |
| ql_check_type_selection | none | 0.0508 | | not gated |
| ql_geo_from_params | none | 0.2888 | | not gated |
| ql_parse_q | none | 0.8564 | | not gated |
| sql_compile_geo_near | none | 6.1114 | | not gated |
| sql_compile_geo_within | none | 15.0175 | | not gated |
| sql_compile_q | none | 4.6239 | | not gated |
| sql_compile_scope_q | none | 0.512 | | not gated |

no baseline for AMD EPYC 7763 64-Core Processor: not gated; commit it with --write
