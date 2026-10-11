# Performance baseline

The `perf-baseline` workflow builds the broker twice in one job, a release
binary with overflow checks off and one with them on, and runs the same
shapes (`dev/perf/baseline.sh`: `shapes.sh` at 1 000 and 100 000 entities,
`fire.sh` for subscription fan-out) against each on the memory and the
PostgreSQL store. The two builds share the runner, the dataset and the
hour, so their ratio is the cost of the checks. The absolute numbers are a
shared GitHub `ubuntu-latest` runner's, not a capacity figure: the
[Performance](performance.md) chapter's dedicated-vCPU runs are those. The
job summary of each run names its commit and its CPU.

## 1. The run

| run | commit | runner | artifact |
|---|---|---|---|
| [38078053835](https://github.com/marek-mraz-jc/Antares-NGSI-LD-Context-Broker/actions/runs/38078053835) | `91a723c` (dev) | GitHub `ubuntu-latest` | `perf-baseline` |
| [38073439536](https://github.com/marek-mraz-jc/Antares-NGSI-LD-Context-Broker/actions/runs/38073439536) | `255ac7c` (branch) | GitHub `ubuntu-latest` | `perf-baseline`, cancelled after vanilla and the overflow e1k pass |

Concurrency 50 for every shape; p99 in milliseconds.

## 2. 1 000 entities, run 38078053835

| store | shape | checks off req/s | checks on req/s | off p99 | on p99 |
|---|---|---|---|---|---|
| memory | query | 548 | 563 | 166.14 | 166.00 |
| memory | query-q | 500 | 510 | 185.75 | 162.92 |
| memory | retrieve | 12798 | 13168 | 12.92 | 12.50 |
| memory | upsert | 1253 | 1263 | 52.80 | 51.77 |
| memory | subscribe | 6759 | 6704 | 26.39 | 25.38 |
| postgres | query | 454 | 565 | 90.26 | 80.30 |
| postgres | query-q | 97 | 97 | 453.53 | 399.16 |
| postgres | retrieve | 1080 | 1200 | 40.96 | 35.39 |
| postgres | upsert | 115 | 132 | 353.28 | 322.43 |
| postgres | subscribe | 316 | 350 | 142.72 | 107.57 |

## 3. 100 000 entities, run 38078053835

| store | shape | checks off req/s | checks on req/s | off p99 | on p99 |
|---|---|---|---|---|---|
| memory | query | 20 | 21 | 13463.07 | 13682.64 |
| memory | query-q | 19 | 19 | 13893.34 | 14000.93 |
| memory | retrieve | 5132 | 5504 | 14.54 | 14.42 |
| postgres | query | 22 | 24 | 1059.17 | 913.74 |
| postgres | query-q | 14 | 13 | 964.80 | 973.98 |
| postgres | retrieve | 31 | 31 | 945.07 | 943.80 |

## 4. Subscription fan-out, run 38078053835

| store | subscriptions | delivered | checks off notifications/s | checks on notifications/s | off PATCH p99 | on PATCH p99 |
|---|---|---|---|---|---|---|
| memory | 1 | 10 of 10 | 1624 | 1514 | 0.7 ms | 0.9 ms |
| memory | 100 | 1000 of 1000 | 13852 | 14488 | 1.8 ms | 0.8 ms |
| memory | 10000 | 100000 of 100000 | 23754 | 24125 | 0.5 ms | 0.5 ms |
| postgres | 1 | 10 of 10 | 89 | 79 | 16.4 ms | 15.4 ms |
| postgres | 100 | 1000 of 1000 | 2051 | 2195 | 96.3 ms | 58.6 ms |
| postgres | 10000 | 100000 of 100000 | 10357 | 10385 | 16.4 ms | 6.4 ms |

## 5. Overflow checks in release

Release builds check integer overflow (`overflow-checks = true` in
`[profile.release]`, T-3552). The rule was: on, unless p99 or throughput
moves by more than 5 %. On the memory store at 1 000 entities, the shapes
that measure the broker rather than the runner, checks on is within 3 %
of checks off in both samples (sample 1: query −2.4 %, query-q +0.9 %,
retrieve −0.1 %, upsert −2.7 %, subscribe 0 %; sample 2: +2.7 %, +2.0 %,
+2.9 %, +0.8 %, −0.8 %). The cells beyond 5 % disagree in sign between the
samples (memory 100 000 entities: −7.7 to −11.4 % in sample 1, 0 to +7.2 %
in sample 2) or rest on ten notifications delivered in under 0.2 s (the
one-subscription fan-out rows), and PostgreSQL moves by up to 25 % between
the two passes of this run and by several times in sample 1 (query-q 214
against 1002 req/s), so they measure the runner. The workflow
keeps measuring the cost: its "checks off" binary is built with
`CARGO_PROFILE_RELEASE_OVERFLOW_CHECKS=false`.

## 6. The budget for a surface

Decided (worker-2, T-3552, owner may override): a surface mounted beside
the NGSI-LD API (`ANTARES_API_SURFACES`) keeps p99 within 10 % and
throughput at 90 % or more of the same request on the NGSI-LD API,
measured in one job as above; a translation layer adds at most 200 µs to
the median.

## Related

- [Performance](performance.md)
- [Conformance](conformance.md)
