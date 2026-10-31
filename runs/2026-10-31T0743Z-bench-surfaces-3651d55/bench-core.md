perf gate (bench, AMD EPYC 7763 64-Core Processor, threshold 15%)

| row | baseline | this run | change | verdict |
|---|---|---|---|---|
| jsonld_compact_page_100 | none | 139.4801 | | not gated |
| ql_check_type_selection | none | 0.0518 | | not gated |
| ql_geo_from_params | none | 0.2935 | | not gated |
| ql_parse_q | none | 0.7758 | | not gated |
| sql_compile_geo_near | none | 6.1249 | | not gated |
| sql_compile_geo_within | none | 14.7179 | | not gated |
| sql_compile_q | none | 4.563 | | not gated |
| sql_compile_scope_q | none | 0.5177 | | not gated |

no baseline for AMD EPYC 7763 64-Core Processor: not gated; commit it with --write
