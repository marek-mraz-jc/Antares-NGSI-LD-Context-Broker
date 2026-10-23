# OGC API - Features compliance

The [OGC API - Features endpoint](ogc.md) runs against the OGC CITE
executable test suite, [ets-ogcapi-features10](https://github.com/opengeospatial/ets-ogcapi-features10),
the suite OGC certification uses. Our own tests (`crates/antares-ogc/tests`)
check each row of `docs/analysis/ogc-features-mapping.md`. The ETS checks the
endpoint from the outside, as a client would. This page records what the
build passes today (T-3747), on every kind of Endpoint (T-3831).

## 1. The run

Job `ogc-compliance-external` in `.github/workflows/surfaces-conformance.yml`
runs on every pull request to `dev`, nightly and on `v*` tags:

1. It takes the `full` broker of the `build` job and starts it on port 9090
   with `ANTARES_API_SURFACES=admin,views,ogc`.
2. It loads `ci/views`, then `ci/views/ogc`, with `dev/views-rig.sh`. The job
   is a matrix over seven views, one per kind of Endpoint (section 7): `all`
   reads the `conformance` Tenant (collection `AirQualityObserved`, three
   seeded Entities with a `location`); the six `ogc-*` views read the `ogc`
   Tenant, which adds the geometry-less class `Reading`.
3. It starts TEAM Engine from the image
   `ogccite/ets-ogcapi-features10:1.8-teamengine-5.7`, pinned by digest
   (`sha256:d884de89…bdaffe`), on the host network.
4. It asks TEAM Engine's REST API for a run of suite `ogcapi-features-1.0`
   against `http://localhost:9090/x/views/<view>/ogc/features` with
   `noofcollections=-1`, as `tests/ogc/run.sh` of joinedcontext-conformance
   (T-0057) does, and keeps the TestNG document it answers with.
5. It gates the run with `dev/ogc-compliance/baseline.py check` against
   `ci/ogc-compliance/<view>.json` (section 3).

The artifact `ogc-compliance-external-<view>` holds the TestNG document, the gate's
Markdown, the broker log and the TEAM Engine log. The job summary shows the
per-class table. The job holds no secret: the broker's `/q` routes stay
inside the runner, the TEAM Engine account `ogctest` is the image's public
test account, and the seed is public test data.

To run it locally, on a host where a 1 GB Tomcat image is fine:

```sh
cargo build -p antares-broker --features views,ogc
ANTARES_HTTP_PORT=9090 ANTARES_API_SURFACES=admin,views,ogc \
  ANTARES_SCHEMA_MAX_BYTES=1048576 target/debug/antares &
dev/views-rig.sh http://localhost:9090
dev/views-rig.sh http://localhost:9090 ci/views/ogc
docker run -d --network host ogccite/ets-ogcapi-features10:1.8-teamengine-5.7
curl -u ogctest:ogctest -H 'Accept: application/xml' -o testng-results.xml \
  'localhost:8080/teamengine/rest/suites/ogcapi-features-1.0/run?iut=http%3A%2F%2Flocalhost%3A9090%2Fx%2Fviews%2Fall%2Fogc%2Ffeatures&noofcollections=-1'
dev/ogc-compliance/baseline.py check testng-results.xml ci/ogc-compliance/all.json
```

## 2. Which parts an ETS tests

| Part | Conformance classes the endpoint declares | ETS |
|---|---|---|
| Part 1 Core (17-069r4) | core, geojson, oas30 | ets-ogcapi-features10 1.8, class `Core` |
| Part 2 CRS by Reference (18-058r1) | crs | ets-ogcapi-features10 1.8, class `Coordinate Reference Systems by Reference` |
| Part 3 Filtering, CQL2 (19-079r2, 21-065r2) | the filter and CQL2 classes, with the `ogc-cql2` crate (T-3753) | none: OGC publishes no ETS for Part 3 or CQL2; the endpoint's own tests cover the F rows |
| Part 4 Create, Replace, Update and Delete (20-002, draft) | create-replace-delete, update, features | none: no ETS tests Part 4; the endpoint's own tests cover the W rows |
| Part 5 Schemas (23-058, draft) | schemas, core-roles-features, feature-references, returnables-and-receivables, sortables, profile-parameter, profile-references | none: no ETS tests Part 5; the endpoint's own tests cover the S rows |

The job pins the 1.8 release; 1.9 is a release candidate. Watch: when ETS 1.9 or the published Parts 4 and 5 (drafts 20-002r1, 23-058r1) appear, re-validate the draft conformance ids `antares_ogc::CONFORMANCE` declares (T-5204).

## 3. The baseline and the regression gate

`ci/ogc-compliance/<view>.json` records every test method of the view's run
that is no TestNG configuration method (71 for view `all` today). Each test is keyed
`Core :: FeaturesLimit.validateFeaturesWithLimitResponse_NumberMatched`
(class, test class, method), with `#2`, `#3` added when a data provider runs a
method again, and status `PASS`, `FAIL` or `SKIP`. `baseline.py check` fails
the job when a test the baseline passes no longer passes: it failed, it was
skipped, or it is gone. A known failure never fails the job, and a run that
holds no test method, or no TestNG document at all, fails it. A new pass is
listed so the baseline can be raised: run `baseline.py write` on the job's
`testng-results.xml` and commit it in the same change as the fix.

`baseline.py --self-test` runs first in the job.

## 4. Pass rate per class

The pass rate counts passed and failed tests; skipped tests stay out of it.
The baseline was written from surfaces-conformance run 38141481939 on
`4efb84c` (T-3747):

| Conformance class | Passed | Failed | Skipped | Pass rate |
|---|---:|---:|---:|---:|
| Core | 43 | 0 | 3 | 100.0 % |
| Coordinate Reference Systems by Reference | 24 | 0 | 1 | 100.0 % |
| **All** | 67 | 0 | 4 | 100.0 % |

The first run (38141079096 on `e2753b4`) failed two Core tests, both fixed
on this branch:

- `FeaturesLimit.validateFeaturesWithLimitResponse_NumberMatched` follows a
  `next` token with another `limit`. The token was bound to the whole query,
  `limit` included, and answered 400. It is bound to the query without
  `limit` now (mapping row C16).
- `FeaturesBBox.boundingBoxParameterDefinition` reads `minItems` and
  `maxItems` of the `bbox` parameter's schema at its top level. The OpenAPI
  document stated them only inside the `oneOf`; it states 4 and 6 at the top
  as well now.

## 5. The skipped tests

| Test | Why it skips |
|---|---|
| `Features.validateFeaturesResponse_TimeStamp`, `FeaturesLimit.validateFeaturesWithLimitResponse_TimeStamp` (twice) | the endpoint sends no `timeStamp`, by design: the member is optional and a response time is no Entity time (mapping row C17) |
| `BBoxCrsParameterInvalid.verifyBboxCrsParameterInvalid` | the collection states no spatial `extent`, which the test needs to build its request: the extent would take a scan of the Tenant's Entities on every collection read |

## 6. The gate goes red

The first run, checked against the baseline, loses two passes and exits 1.
A deliberate break proved the same in the job: run 38141708488 (`b1d1f11`) on a
throwaway branch, which dropped the top-level `minItems` from the `bbox`
schema again, went red on `Core :: FeaturesBBox.boundingBoxParameterDefinition`.

## 7. Every kind of Endpoint

The platform puts six kinds of Endpoint in front of this surface. Each is a
view, and each runs the ETS on its own (`ci/views/ogc`, `ogc` Tenant;
`all` is the conformance Tenant's star view). The table counts passed,
failed and skipped tests, from local runs of the pinned image on
`agent/worker-7/T-3831` (2026-10-12):

| view | Endpoint kind | Core | CRS by Reference |
|---|---|---|---|
| `all` | star, conformance Tenant | 43 / 0 / 3 | 24 / 0 / 1 |
| `ogc-public` | public: a type list, reads only | 43 / 0 / 3 | 24 / 0 / 1 |
| `ogc-internal` | internal: the same with the Part 4 writes | 43 / 0 / 3 | 24 / 0 / 1 |
| `ogc-filtered` | filtered by the view's `q` (`NO2>25`) | 43 / 0 / 3 | 24 / 0 / 1 |
| `ogc-star` | star (ADR-0023 rule 12): both classes | 75 / 0 / 6 | 43 / 2 / 2 |
| `ogc-projection` | projection: `temperature` and `location` | 43 / 0 / 3 | 24 / 0 / 1 |
| `ogc-cube` | data cube observations: `Reading`, no geometry | 43 / 0 / 3 | 22 / 2 / 1 |

The two failures of `ogc-star` and `ogc-cube` are the same ETS defect, kept as
known failures in their baselines:
`FeatureCrsParameterTransform.verifyFeatureCrsParameterTransformWithoutCrsParameter`
(twice) throws a `NullPointerException` on a `Reading` Feature, whose
`"geometry": null` RFC 7946 section 3.2 allows. The endpoint answers 200 with
`Content-Crs` as the requirement asks; the test reads coordinates that a
Feature without a geometry does not have.

No ETS exists for Parts 3, 4 and 5 (section 2). The matrix test
`crates/antares-ogc/tests/ogc/matrix.rs` runs every Part on the same six
kinds, one cell per Part and kind. A cell holds, or the write cell of a view
without writes asserts its 405 with `Allow`. In every column no feature,
queryable or schema shows an attribute the view hides, and a filter on one
is the same 400 as an unknown name.

## Related

- [OGC API - Features endpoint of a view](ogc.md)
- [OData compliance](odata-compliance.md)
- [Conformance](conformance.md)
- `docs/analysis/ogc-features-mapping.md`
- `docs/analysis/surfaces-conformance.md`
