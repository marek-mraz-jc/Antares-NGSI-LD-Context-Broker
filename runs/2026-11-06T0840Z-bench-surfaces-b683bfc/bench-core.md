perf gate (bench, AMD EPYC 9V74 80-Core Processor, threshold 15%)

| row | baseline | this run | change | verdict |
|---|---|---|---|---|
| jsonld_compact_page_100 | none | 115.161 | | not gated |
| ql_check_type_selection | none | 0.0615 | | not gated |
| ql_geo_from_params | none | 0.2567 | | not gated |
| ql_parse_q | none | 0.6743 | | not gated |
| sql_compile_geo_near | none | 5.4419 | | not gated |
| sql_compile_geo_within | none | 12.9876 | | not gated |
| sql_compile_q | none | 3.7763 | | not gated |
| sql_compile_scope_q | none | 0.4394 | | not gated |

no baseline for AMD EPYC 9V74 80-Core Processor: not gated; commit it with --write
