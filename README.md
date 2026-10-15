# Antares

**An NGSI-LD Context Broker in Rust.** ETSI CIM 009 V1.9.1, one native
binary, about 35 MiB of memory under the full conformance suite, and a
WebAssembly build that runs the same broker inside a web page.

📖 **Docs:** <https://antaresbroker.joinedcontext.com/>

🚀 **Demo:** <https://antaresbroker.joinedcontext.com/demo/> — the context
broker running in your browser, yes in the browser 😅

[![ci](https://github.com/joinedcontext/Antares-NGSI-LD-Context-Broker/actions/workflows/ci.yml/badge.svg)](https://github.com/joinedcontext/Antares-NGSI-LD-Context-Broker/actions/workflows/ci.yml)
[![strict](https://github.com/joinedcontext/Antares-NGSI-LD-Context-Broker/actions/workflows/strict.yml/badge.svg)](https://github.com/joinedcontext/Antares-NGSI-LD-Context-Broker/actions/workflows/strict.yml)
[![ETSI conformance](https://img.shields.io/endpoint?url=https%3A%2F%2Fantaresbroker.joinedcontext.com%2Freports%2Fbadge.json)](https://antaresbroker.joinedcontext.com/reports/latest/)
[![NGSI-LD conformance](https://img.shields.io/endpoint?url=https%3A%2F%2Fantaresbroker.joinedcontext.com%2Freports%2Fbadge-postgres.json&label=NGSI-LD%20conformance)](#etsi-conformance-results)
[![unit tests](https://img.shields.io/endpoint?url=https%3A%2F%2Fantaresbroker.joinedcontext.com%2Freports%2Fbadge-unit.json)](https://antaresbroker.joinedcontext.com/reports/unit/)
[![coverage](https://img.shields.io/endpoint?url=https%3A%2F%2Fantaresbroker.joinedcontext.com%2Freports%2Fcoverage-badge.json)](https://antaresbroker.joinedcontext.com/reports/coverage/)
[![docs](https://img.shields.io/badge/docs-antaresbroker.joinedcontext.com-blue)](https://antaresbroker.joinedcontext.com/)
[![license: EUPL-1.2](https://img.shields.io/badge/license-EUPL--1.2-blue)](LICENSE)
[![release](https://img.shields.io/github/v/release/joinedcontext/Antares-NGSI-LD-Context-Broker?include_prereleases)](https://github.com/joinedcontext/Antares-NGSI-LD-Context-Broker/releases)

## Why Antares

- **Small.** About 35 MiB average RSS while the complete ETSI suite runs,
  about 9 MiB idle. The whole store ladder fits where a JVM heap alone
  would not.
- **Conformant.** 1823/1823 ETSI CIM 009 V1.9.1 test cases green in every
  native store mode, and 1811/1811 in the browser build, where the MQTT
  cases have no socket to run against
  ([per-store report with Robot drill-down](https://antaresbroker.joinedcontext.com/reports/latest/)).
  The ledger covers the whole spec text, one file per clause, in `docs/spec/`.
- **Runs anywhere.** Zero infrastructure by default, PostgreSQL for
  production, NATS JetStream for scale-out, and a 4 MB wasm artifact that
  serves `/ngsi-ld/v1/*` from a Service Worker with nothing installed.
- **Tenants without a ceiling.** A tenant is a row in one shared schema,
  isolated by `tenant_id` and Row-Level Security, and exists from the
  first request that names it. Nothing in the broker caps the count, so
  one deployment can hand every employee, department and use case a
  tenant of its own, 100,000 of them on one Postgres cluster.

## ETSI conformance results

The latest full run of the ETSI NGSI-LD test suite (CIM 009 V1.9.1), one
cell per store mode. The badges read the live report, so they always show
the newest run.

The suite in [`ngsi-ld-test-suite/`](ngsi-ld-test-suite/) started from the
[ETSI NGSI-LD test suite](https://forge.etsi.org/rep/cim/ngsi-ld-test-suite)
and was corrected and extended; no test was weakened.

| Store | Setup | Result |
|---|---|---|
| `memory` | in-RAM, zero dependencies | [![memory](https://img.shields.io/endpoint?url=https%3A%2F%2Fantaresbroker.joinedcontext.com%2Freports%2Fbadge-memory.json)](https://antaresbroker.joinedcontext.com/reports/latest/) |
| `file` | redb file store | [![file](https://img.shields.io/endpoint?url=https%3A%2F%2Fantaresbroker.joinedcontext.com%2Freports%2Fbadge-file.json)](https://antaresbroker.joinedcontext.com/reports/latest/) |
| `postgres` | PostGIS | [![postgres](https://img.shields.io/endpoint?url=https%3A%2F%2Fantaresbroker.joinedcontext.com%2Freports%2Fbadge-postgres.json)](https://antaresbroker.joinedcontext.com/reports/latest/) |
| `timescale` | PostGIS + TimescaleDB | [![timescale](https://img.shields.io/endpoint?url=https%3A%2F%2Fantaresbroker.joinedcontext.com%2Freports%2Fbadge-timescale.json)](https://antaresbroker.joinedcontext.com/reports/latest/) |
| `postgres-nats` | PostGIS + NATS JetStream, ten containers rolled during the run | [![postgres-nats](https://img.shields.io/endpoint?url=https%3A%2F%2Fantaresbroker.joinedcontext.com%2Freports%2Fbadge-postgres-nats.json)](https://antaresbroker.joinedcontext.com/reports/latest/) |
| `timescale-nats` | TimescaleDB + NATS JetStream, same rolling fleet | [![timescale-nats](https://img.shields.io/endpoint?url=https%3A%2F%2Fantaresbroker.joinedcontext.com%2Freports%2Fbadge-timescale-nats.json)](https://antaresbroker.joinedcontext.com/reports/latest/) |
| `wasm-file` | WebAssembly build over the file store, MQTT excluded | [![wasm-file](https://img.shields.io/endpoint?url=https%3A%2F%2Fantaresbroker.joinedcontext.com%2Freports%2Fbadge-wasm-file.json)](https://antaresbroker.joinedcontext.com/reports/latest/) |
| unit + integration | `cargo test` | [![unit](https://img.shields.io/endpoint?url=https%3A%2F%2Fantaresbroker.joinedcontext.com%2Freports%2Fbadge-unit.json)](https://antaresbroker.joinedcontext.com/reports/unit/) |

## Quickstart

```bash
docker run --rm -p 9090:9090 ghcr.io/joinedcontext/antares-broker:latest
```

```bash
curl -i -X POST localhost:9090/ngsi-ld/v1/entities \
  -H 'Content-Type: application/ld+json' \
  -d '{
    "id": "urn:ngsi-ld:TemperatureSensor:001",
    "type": "TemperatureSensor",
    "temperature": {"type": "Property", "value": 21.5, "unitCode": "CEL"},
    "@context": "https://uri.etsi.org/ngsi-ld/v1/ngsi-ld-core-context-v1.9.jsonld"
  }'
# HTTP/1.1 201 Created

curl -s 'localhost:9090/ngsi-ld/v1/entities?type=TemperatureSensor'
```

Without Docker, `cargo run -p antares-broker` serves the same API on port
9090 (Rust 1.97 or newer). Continue with the
[Getting started](docs/src/getting-started.md) chapter.

## Store modes

One binary, one setting (`ANTARES_STORE`), the same API in every mode.

| Mode | Backend | Durability | Config |
|---|---|---|---|
| `memory` (default) | in-memory maps | none | |
| `file` | memory + [redb](https://www.redb.org/) write-through shadow, fsync before every ack | process restarts | `ANTARES_DATA_DIR` on a mounted volume |
| `postgres` | PostgreSQL + PostGIS | WAL, ordinary Postgres backup and PITR | `ANTARES_DATABASE_URL` |
| `timescale` | PostgreSQL + TimescaleDB for history | WAL | `ANTARES_DATABASE_URL` |

`memory` for tests and demos, `file` for a durable single node without a
database, `postgres` for production, `timescale` when temporal queries
dominate. `ANTARES_BUS=nats` adds JetStream for multi-pod scale-out and
rolling updates; MQTT notifications are built in and chosen per
subscription endpoint. Details, measured costs and backup procedures:
[Storage drivers](docs/src/storage.md) and [Operations](docs/src/operations.md).

```bash
# durable single node, no Postgres
docker run --rm -p 9090:9090 -e ANTARES_STORE=file -e ANTARES_DATA_DIR=/data \
  -v antares-data:/data ghcr.io/joinedcontext/antares-broker:latest

# postgres / timescale
docker run --rm -p 9090:9090 -e ANTARES_STORE=postgres \
  -e ANTARES_DATABASE_URL=postgresql://antares:antares@db:5432/antares \
  ghcr.io/joinedcontext/antares-broker:latest

# local stacks: broker + PostGIS + NATS + mosquitto
docker compose -f compose-files/docker-compose.yml up       # one broker
docker compose -f compose-files/docker-compose-ha.yml up    # two replicas + haproxy + NATS
```

Image tags: `:dev` is the latest green `dev`, `:dev-<run>` one CI run,
`:0.1.3` one release, `:0.1` the newest patch of a minor, `:latest` the
latest release. Images are multi-arch (amd64, arm64). The
role-split fleet (`--roles api,matcher,notifier,temporal,registry`) and
the Kubernetes manifests are in [Deployment](docs/src/deployment.md).

## Browser build

The broker compiles to `wasm32-unknown-unknown` and answers `/ngsi-ld/v1/*`
from a Service Worker; `www/index.html` is the playground.

```bash
./dev/install-wasm-tools.sh   # wasm-bindgen + wasm-opt
./dev/wasm-build.sh           # → www/pkg
node www/node-shim.mjs 9090   # the same .wasm behind a TCP port
./dev/wasm-test.sh            # Node smoke + headless-Chromium page test
```

Scope: memory store and local bus only. See [Browser & WebAssembly](docs/src/wasm.md).

## Observability

```bash
curl -s localhost:9090/q/health    # liveness, store mode, bus state
curl -s localhost:9090/q/ready     # readiness: store ping + bus connected
curl -s localhost:9090/q/metrics   # Prometheus, antares_ prefixed
```

Traces and logs export over OTLP/HTTP when `ANTARES_OTLP_ENDPOINT` is set.
Every route under `/q/` is in the [Admin API](docs/src/admin-api.md) chapter.

## Conformance

The [ETSI NGSI-LD test suite](https://forge.etsi.org/rep/cim/ngsi-ld-test-suite)
is vendored in `ngsi-ld-test-suite/` and runs in CI against every store
mode, including a ten-container role-split fleet that rolls under the
suite and the browser build behind a Node shim. The vendored copy carries a
few test-side fixes, each proven from the clause text and listed in
[docs/upstream/etsi-raises.md](docs/upstream/etsi-raises.md); no test is
weakened to fit the broker.

```bash
cargo test --workspace -j 2                 # unit and integration tests
dev/etsi-local.sh                           # workspace tests + the suite, memory store
STORE=postgres dev/etsi-local.sh            # the store mode you are touching
```

How to read the matrix and the ledger: [Conformance](docs/src/conformance.md).

## Documentation

The book is published at <https://antaresbroker.joinedcontext.com/>
and built from `docs/src`. Maintainers start at
[ARCHITECTURE.md](ARCHITECTURE.md).

| Chapter | Covers |
|---|---|
| [Getting started](docs/src/getting-started.md) | install, first entity, first subscription, first federation pair |
| [Configuration](docs/src/configuration.md) | every `ANTARES_*` variable, with defaults |
| [Deployment](docs/src/deployment.md) | Docker, compose stacks, Kubernetes, the role split |
| [Subscriptions](docs/src/subscriptions.md) | HTTP and MQTT delivery, retries, dead letters |
| [Temporal API](docs/src/temporal.md) | recording, querying, aggregation, retention |
| [Federation](docs/src/federation.md) | registrations, forwarding, loop protection |
| [Operations](docs/src/operations.md) | health, tenants, backup and restore, bulk load, rolling updates |
| [Storage drivers](docs/src/storage.md) | the store ladder, the temporal driver, migrations |
| [Performance](docs/src/performance.md) | the weekly shape and scale runs and how to reproduce them |
| [Shared crates](docs/src/shared-crates.md) | the parser, query engine and matcher as libraries |
| [Extending Antares](docs/src/extending.md) | features, the driver registry, hooks, adding a backend |
| [Decisions](docs/adr/README.md) | the architecture decision records |

## Design targets

| Dimension | Target |
|---|---|
| Entities | 100,000,000 current-state, one PostgreSQL cluster |
| Tenants | 10,000, one shared schema, `tenant_id` + Row-Level Security |
| Subscriptions | 100,000 per broker, HTTP + MQTT |
| CSource registrations | 100,000+ per broker, index-shaped matching, bounded fan-out |
| HA | stateless broker pods, NATS JetStream, Postgres primary/replica |

The weekly scale run measures a fixed fraction of these rows on rented
hardware and publishes the broker's and Postgres's memory and CPU per
phase; the [Performance](docs/src/performance.md) chapter says how each
number is produced.

## The NGSI-LD crates

`ngsild-model`, `ngsild-jsonld`, `ngsild-ql`, `ngsild-matcher` and `ngsild-residual` (folders
`crates/antares-*`, `antares_*` inside this workspace through `package =`, T-3962) hold the
NGSI-LD data model, JSON-LD expansion, the query languages, subscription matching and the
residual evaluator with no server, store or network in their default features (role `ngsild`,
ADR-0027). Each has a README with a runnable example and the CIM 009 clauses it implements, and
builds from its packaged files alone (`cargo publish --dry-run` in `workspace.yml`). They are
published to crates.io by `.github/workflows/release-crates.yml` from a `v*` tag only, through
crates.io trusted publishing (no stored token) with SLSA provenance on every `.crate`.

Yanking a bad version: run `cargo yank --version <x.y.z> <crate>` with an owner's crates.io login
for every crate of the release that carries the defect, open an issue that names the version and
the defect, and release the fix as a new patch version from a new tag. A yank stops new lockfiles
from choosing the version; it does not delete it or break a build that already locked it.

## Repository layout

```
crates/antares-model      NGSI-LD types
crates/antares-ql         q= / scopeQ / geoQ parsers and evaluator
crates/antares-jsonld     @context cache and core-context fast path
crates/antares-sql        storage drivers, SQL compiler, migrations
crates/antares-bus        change-event bus: in-process or NATS JetStream
crates/antares-api        HTTP binding (axum) and the operations
crates/antares-broker     composition root, the `antares` binary
crates/antares-wasm       the browser build
docs/                     conformance ledger, ADRs, user book source
dev/                      run and test scripts
compose-files/            local stacks
```

## Related brokers

Antares is a compliant peer of Orion-LD, Scorpio and Stellio, not a fork of
any of them: it shares no code with them and federates with them over the
standard distributed-operations API. Where it fits and how it compares:
[Ecosystem](docs/src/ecosystem.md).

## Contributing

Changes are accepted into `dev` only. Fork, branch from `dev`, open the
pull request into `dev`; one aimed at `main` fails its first check and
gets retargeted. `main` holds released code and moves only when the
maintainers promote `dev` into it, and releases are the `v*` tags cut
there. Commits carry a DCO sign-off (`git commit -s`), one change each.

The whole process, from the first commit to a release, is in
[CONTRIBUTING.md](CONTRIBUTING.md). Report a vulnerability the way
[SECURITY.md](SECURITY.md) describes, not in a public issue.

## License

[EUPL-1.2](LICENSE), the European Union Public Licence. Commercial
licensing: contact@marek-mraz.com.
