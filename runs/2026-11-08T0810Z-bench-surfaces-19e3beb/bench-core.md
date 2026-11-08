perf gate (bench, AMD EPYC 7763 64-Core Processor, threshold 15%)

| row | baseline | this run | change | verdict |
|---|---|---|---|---|
| jsonld_compact_page_100 | none | 134.9782 | | not gated |
| ql_check_type_selection | none | 0.0508 | | not gated |
| ql_geo_from_params | none | 0.2942 | | not gated |
| ql_parse_q | none | 0.7811 | | not gated |
| sql_compile_geo_near | none | 6.101 | | not gated |
| sql_compile_geo_within | none | 14.6321 | | not gated |
| sql_compile_q | none | 5.0218 | | not gated |
| sql_compile_scope_q | none | 0.4962 | | not gated |

no baseline for AMD EPYC 7763 64-Core Processor: not gated; commit it with --write
