perf gate (bench, AMD EPYC 7763 64-Core Processor, threshold 15%)

| row | baseline | this run | change | verdict |
|---|---|---|---|---|
| jsonld_compact_page_100 | none | 127.7422 | | not gated |
| ql_check_type_selection | none | 0.0513 | | not gated |
| ql_geo_from_params | none | 0.2949 | | not gated |
| ql_parse_q | none | 0.7608 | | not gated |
| sql_compile_geo_near | none | 1.1393 | | not gated |
| sql_compile_geo_within | none | 1.7821 | | not gated |
| sql_compile_q | none | 4.9361 | | not gated |
| sql_compile_scope_q | none | 0.5008 | | not gated |

no baseline for AMD EPYC 7763 64-Core Processor: not gated; commit it with --write
