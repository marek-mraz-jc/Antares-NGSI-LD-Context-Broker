perf gate (bench, AMD EPYC 9V45 96-Core Processor, threshold 15%)

| row | baseline | this run | change | verdict |
|---|---|---|---|---|
| jsonld_compact_page_100 | none | 90.5572 | | not gated |
| ql_check_type_selection | none | 0.036 | | not gated |
| ql_geo_from_params | none | 0.2099 | | not gated |
| ql_parse_q | none | 0.5304 | | not gated |
| sql_compile_geo_near | none | 0.852 | | not gated |
| sql_compile_geo_within | none | 1.3716 | | not gated |
| sql_compile_q | none | 3.4697 | | not gated |
| sql_compile_scope_q | none | 0.3297 | | not gated |

no baseline for AMD EPYC 9V45 96-Core Processor: not gated; commit it with --write
