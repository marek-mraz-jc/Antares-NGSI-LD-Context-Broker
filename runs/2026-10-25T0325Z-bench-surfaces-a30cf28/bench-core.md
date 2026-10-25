perf gate (bench, AMD EPYC 7763 64-Core Processor, threshold 15%)

| row | baseline | this run | change | verdict |
|---|---|---|---|---|
| jsonld_compact_page_100 | none | 129.1283 | | not gated |
| ql_check_type_selection | none | 0.0524 | | not gated |
| ql_geo_from_params | none | 0.2959 | | not gated |
| ql_parse_q | none | 0.7915 | | not gated |
| sql_compile_geo_near | none | 1.0912 | | not gated |
| sql_compile_geo_within | none | 1.7931 | | not gated |
| sql_compile_q | none | 4.5502 | | not gated |
| sql_compile_scope_q | none | 0.5109 | | not gated |

no baseline for AMD EPYC 7763 64-Core Processor: not gated; commit it with --write
