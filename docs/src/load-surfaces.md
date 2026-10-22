# Load on every surface

`dev/perf/surfaces.sh` (T-4889) seeds PostgreSQL with 100 000 synthetic entities over ten types,
every one with a Point, 30 days of `pm25` (four instances a day) on 500 AirQualityObserved and 5 000
Events, then holds one k6 shape at a time at its rate for 30 s (open model,
`dev/perf/k6-surfaces.js`). The vanilla binary (default features) runs the NGSI-LD shapes; the
platform binary (`full,jc-policy,linkml,parquet,tabular,ical`) runs them again and adds the view
root, OData, OGC API - Features, SensorThings, iCalendar, MCP and a WebSocket fan-out of 1 000
subscribers. `dev/perf/surfaces_gate.py` judges each shape against `dev/perf/surfaces-budgets.json`
(p95, p99, errors, throughput, each with its reason) and against `ci/perf-baseline/surfaces.json`
when that baseline was measured on the same machine class. The `load-surfaces` workflow runs it
every Sunday at 04:23Z and fails on a breach.

The platform's own surfaces (the Portal grid, the Portal's live view, MQTT ingest through the edge
broker) load dev at night from joinedcontext-conformance `tests/k6/night.sh`.

## 1. Runs

| run | commit | machine | note |
|---|---|---|---|
| local, 2026-10-20 22:00Z | `53ab566e` + the T-4889 branch | sandbox, 12 cores, PostGIS 17-3.5 in Docker | one pass, no repetition; the baseline in `ci/perf-baseline/surfaces.json` |
| local, 2026-10-21 22:30Z | `3c43f323` (dev) | sandbox, 12 cores, PostGIS 17-3.5 in Docker | `SHAPES=ngsi-near,ngsi-within` only, after T-4852 (T-4918) |
| local, 2026-10-22 00:55Z | dev `a9bd198b` (the fix landed with T-5062) | sandbox, 16 cores, PostGIS 17-3.5 in Docker | `SHAPES=ogc-items` only, one pass; another machine than the baseline, so not added to it |

The first `load-surfaces` run on `ubuntu-latest` adds its run id here and its baseline replaces the
sandbox one (`surfaces_gate.py baseline --machine github-ubuntu-latest`). The PostgreSQL column of
this run is left out: it summed the RSS of PostgreSQL's processes, which counts shared buffers once
per process; the sampler now reads the container's cgroup (`docker stats`).

## 2. Results

| shape | rate/s | vanilla p95 / p99 ms | platform p95 / p99 ms | platform over vanilla (p95) | platform broker CPU % / RSS MB | budget p95 / p99 | verdict |
|---|---|---|---|---|---|---|---|
| ngsi-id | 200 | 5.19 / 8.34 | 10.36 / 23.69 | 2.0x | 12.0 / 30.4 | 50 / 150 | ok |
| ngsi-type | 100 | 7.31 / 8.76 | 19.54 / 41.36 | 2.7x | 10.5 / 30.8 | 50 / 150 | ok |
| ngsi-q | 100 | 30.14 / 35.97 | 62.87 / 226.73 | 2.1x | 12.1 / 29.9 | 200 / 500 | ok |
| ngsi-near | 50 | 10.09 / 25.85 | 23.46 / 93.68 | 2.3x | 4.9 / 53.6 | 200 / 500 | ok (2026-10-21 run; was 132.91 / 232.31 and 562.99 / 939.71, see section 5) |
| ngsi-within | 50 | 8.97 / 95.69 | 28.19 / 94.53 | 3.1x | 4.8 / 29.4 | 200 / 500 | ok |
| ngsi-attrs | 50 | 22.28 / 27.21 | 25.68 / 42.31 | 1.2x | 8.4 / 27.5 | 50 / 150 | ok |
| temporal-lastn | 50 | 13.48 / 21.53 | 22.37 / 47.85 | 1.7x | 4.4 / 27.3 | 50 / 150 | ok |
| temporal-between | 50 | 8.66 / 11.7 | 22.59 / 44.88 | 2.6x | 4.6 / 26.9 | 200 / 500 | ok |
| upsert-100 | 5 | 51.63 / 65.89 | 44.04 / 92.94 | 0.9x | 2.8 / 32.6 | 500 / 1000 | ok |
| view-type | 100 | n/a (platform only) | 7.78 / 10.11 |  | 20.3 / 35.2 | 50 / 150 | ok |
| odata | 50 | n/a (platform only) | 274.73 / 400.77 |  | 122.7 / 179.4 | 200 / 500 | breach: p95_ms 274.73 > 200 |
| ogc-items | 50 | n/a (platform only) | 30.05 / 47.54 |  | 13.3 / 45.0 | 200 / 500 | ok (T-4920 rerun; before: p95 9 770 ms, p99 10 001 ms, 1.5 % timeouts, 33 of 50 req/s at 354 % CPU) |
| ogc-page | 50 | n/a (platform only) | 1.3 / 2.5 |  | 2.7 / 227.8 | 200 / 500 | breach: errors 100.0 % > 0.1 % |
| sta-things | 20 | n/a (platform only) | 128.28 / 166.76 |  | 3.9 / 161.8 | 200 / 500 | ok |
| sta-observations | 50 | n/a (platform only) | 14.73 / 26.24 |  | 8.6 / 131.7 | 200 / 500 | ok |
| ical | 2 | n/a (platform only) | 665.98 / 710.84 |  | 42.9 / 119.4 | 2000 / 4000 | ok |
| mcp | 50 | n/a (platform only) | 57.25 / 83.0 |  | 11.0 / 119.3 | 200 / 500 | ok |
| ws-fanout | 200 | n/a (platform only) | 30030 / 30524 |  | 79.6 / 283.8 | 250 / 1000 | breach: p95_ms 30030 > 250; p99_ms 30524 > 1000; errors 77.883 % > 0.1 % |

`odata` (T-4919): each read counted the Entities lacking the `$orderby` key, a second scan of the
type beside the read itself, though `level ge a` already rules them out; and the store handed the
broker every match whole (about 1 000 Entities) to sort, read each one's key twice per comparison and
cloned it once more for the view. Now a key the `$filter` compares to a literal skips that count, the
sort reads each key once, and an ordered read the store decides takes only the ordering members and
then reads its page whole by id. Same seeded 100 000 Entities, same host (load average about 40,
so the absolute numbers are not the runner's): broker CPU 21.0 to 9.2 ms per request; at 50/s
p95 3 757 to 506 ms (the old build saturated), broker CPU 94 to 50 %, RSS 222 to 112 MB. The row
above is the run that found it; the next load-surfaces run replaces it.

`ogc-page` asked an `offset` the OGC surface does not define (its paging is the `next` cursor); the
shape now follows the first page's `next` link, as a GIS client pages; a 2 000-entity recheck answered
every page (p95 10.8 ms, p99 34.3 ms at 50/s).

## 3. Breaches

Each breach of this run is a task: `ngsi-near` on the platform build (4.2x vanilla, T-4918, fixed: section 5), `odata` p95 (T-4919, fixed: see the note under the table),
`ogc-items` (bbox with a CQL2 filter on 10 556 AirQualityObserved: 10 s timeouts at 354 % CPU, T-4920; fixed, see below), and
the WebSocket fan-out (20 % of 300 050 owed notifications delivered within the hold, 30 s behind,
no `x-gap` sent, T-4921).

`ogc-items` cause (T-4920): under a view's attribute rules the broker evaluated a client geoquery
on the Entity as shown, so the store got no area. Each `bbox` read sends `count=true`, so the
broker then read the whole type (10 556 rows in pages of 2 049) for every request. A geoquery on a
GeoProperty that every rule of the selected types shows now reaches the store as T-4289 did for
`q` (the code landed with T-5062, `filter_entities_paged`; its test is
`a_geoquery_on_a_shown_location_is_paged_in_the_store_and_a_hidden_one_is_not`); on a hidden
GeoProperty it still runs on the shown Entity. One request fell from about 400 ms to 60 ms.

## 4. Proof the gate goes red

`surfaces_gate.py --selftest` fails a planted slowdown, a planted error rate, half the rate and a
shape that did not run, and passes the clean run. A throwaway branch that sleeps 60 ms in Query
Entities breaks `ngsi-type` on the real binary (section 1 of T-4889).

## 5. ngsi-near after T-4852

The 2026-10-20 run measured `near` before T-4852 landed (`37215069`). Then the broker's role
(`antares_app`) read every geoquery through the tables' row-level security, where no PostGIS
operator is leakproof, so the GIST index on `location` never served as an index condition: each
`near` query read every row of the Tenant and cast each one to `geography` for `ST_DWithin`. `within`
pays the same scan but its per-row test is a plain planar check, which is why `near` alone breached.
T-4852 ANDs every geoquery with a geohash cover over `entities.location_cell` (migration 0015,
leakproof btree text comparisons); a 500 m radius becomes a few cells and `ST_DWithin` only checks
the rows inside them. The 2026-10-21 rerun (section 1) gives `near` at 50/s p95 10.09 / p99 25.85 ms
vanilla and 23.46 / 93.68 ms platform, `within` 10.86 / 25.99 and 24.45 / 81.87 ms. The platform build
keeps its 2.3x over vanilla on both shapes alike, so nothing specific to `near` is left.
`crates/antares-sql/tests/pg_explain.rs` (`every_query_shape_reads_an_index_under_rls`, case
"geo near") holds the plan: a 500 m `near` as `antares_app` must read `i_entities_location_cell`.

## Related

- [Performance](performance.md)
- [Performance baseline](perf-baseline.md)
