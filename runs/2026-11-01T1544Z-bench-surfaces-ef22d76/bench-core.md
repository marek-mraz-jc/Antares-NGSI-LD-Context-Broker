perf gate (bench, AMD EPYC 7763 64-Core Processor, threshold 15%)

| row | baseline | this run | change | verdict |
|---|---|---|---|---|
| jsonld_compact_page_100 | none | 133.4042 | | not gated |
| ql_check_type_selection | none | 0.0672 | | not gated |
| ql_geo_from_params | none | 0.3015 | | not gated |
| ql_parse_q | none | 0.7755 | | not gated |
| sql_compile_geo_near | none | 6.1425 | | not gated |
| sql_compile_geo_within | none | 15.0164 | | not gated |
| sql_compile_q | none | 4.5139 | | not gated |
| sql_compile_scope_q | none | 0.5407 | | not gated |

no baseline for AMD EPYC 7763 64-Core Processor: not gated; commit it with --write
