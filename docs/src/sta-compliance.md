# SensorThings API compliance

The [SensorThings API endpoint](sta.md) runs against the OGC CITE
executable test suite for SensorThings,
[ets-sta10](https://github.com/opengeospatial/ets-sta10), the suite OGC
certification uses. Our own tests (`crates/antares-sta/tests/sta`) check
each row of `docs/analysis/sta-mapping.md` and that every navigation link
answers. The ETS checks the endpoint from the outside, as a client would.
This page records what the build passes today (T-4878) and why each other
test does not pass.

## 1. The suite

| | |
|---|---|
| ETS | ets-sta10 1.5, the latest release (`opengeospatial/ets-sta10` tags 1.1 to 1.5) |
| Image | `ogccite/ets-sta10:1.5-teamengine-6.0.0-RC2@sha256:e1ec8dc3508ea3355f059abee98737f5d2c00434aeffa298d80ea04361c93db3`, the only tag OGC publishes for it |
| Standard | OGC SensorThings API Part 1: Sensing 1.0 (15-078r6) |
| TEAM Engine suite | `sta10`, run with `ics=3`: the three classes below |

OGC publishes no ETS for SensorThings 1.1 (18-088), the version the
endpoint serves (`/sta/v1.1`). 1.1 adds members to the 1.0 entities and
keeps its requirement classes, so the 1.0 suite is the one that applies.

| ETS class | 1.0 requirement classes | The endpoint |
|---|---|---|
| Conformance Level 1, Sensing Core (`Capability1Tests`) | datamodel, resource-path, request-data (read) | declares all three in `serverSettings.conformance` |
| Conformance Level 2, Create-Update-Delete (`Capability2Tests`) | create-update-delete | not served: read only |
| Conformance Level 3, Filtering Extension (`Capability3Tests`) | request-data query options | served; the ETS cannot reach it (section 4) |

## 2. The run

Job `sta-compliance-external` in `.github/workflows/surfaces-conformance.yml`
runs on every pull request to `dev`, nightly and on `v*` tags:

1. It takes the `full` broker of the `build` job and starts it on port 9090
   with `ANTARES_API_SURFACES=admin,views,sta`.
2. `dev/views-rig.sh` loads `ci/views/sta`: view `sta` of Tenant `sta`
   (`AirQualityObserved`, `Device`, `PointOfInterest`; reads only), five
   synthetic Entities, and an earlier `pm10` and `location` of
   `AirQualityObserved:station-1` from `history/sta.json`. Every one of the
   eight sets holds a resource.
3. TEAM Engine runs the suite against
   `http://localhost:9090/x/views/sta/sta/v1.1` and answers a TestNG document.
4. `dev/ogc-compliance/baseline.py check` compares it with
   `ci/sta-compliance-baseline.json`, the gate the OGC API - Features lane
   uses ([OGC API - Features compliance](ogc-compliance.md) section 3): red
   when a test the baseline passes fails, is skipped or is gone; a new pass
   is listed so the baseline can be raised in the same change.

The artifact `sta-compliance-external` holds the TestNG document, the gate's
table, the broker log and the TEAM Engine log for 14 days. A nightly red
files an issue.

## 3. Pass rate per class

From a local run of the pinned image on `agent/worker-t4878/T-4878`
(2026-10-21), which wrote the baseline:

| Conformance class | Passed | Failed | Skipped |
|---|---:|---:|---:|
| Conformance Level 1 | 3 | 3 | 0 |
| Conformance Level 2 | 0 | 0 | 8 |
| Conformance Level 3 | 0 | 0 | 8 |

The first run passed one test (`checkServiceRootUri`). This change fixed
three defects the ETS found:

- `readEntitiesAndCheckResponse`: a Location carried no `Things` or
  `HistoricalLocations` navigation link, a Sensor, ObservedProperty and
  FeatureOfInterest none, a Datastream no `Sensor` or `ObservedProperty`,
  an Observation no `FeatureOfInterest` (15-078r6 section 8.2, each
  entity's relations), and only a Thing's and a Datastream's links
  answered. Every link answers now (section 1 of [the endpoint page](sta.md)).
  An Observation also lacked `resultTime`, which the standard makes
  mandatory with `null` allowed; it is `null` when unknown now.
- `readNonexistentEntity`: `Sensors(9223372036854775807)` answered `400`,
  the view's refusal of an id that is no URI. A key that is no URI is the
  `404` of an unknown resource now.
- A Thing without a `location` answered `404` on its own
  `HistoricalLocations` link; it answers an empty list now.

## 4. The tests that do not pass

| Test | Status | Why | Decision |
|---|---|---|---|
| Level 1 `readEntityAndCheckResponse`, `readPropertyOfEntityAndCheckResponse`, `checkResourcePaths` | FAIL | The ETS reads `@iot.id` with `getLong` and stops: `JSONObject["@iot.id"] is not a long`. 15-078r6 table 2 types `@iot.id` as ANY; the endpoint's ids are the Entity URNs and ids derived from them (sta-mapping rule 2). Upstream issue [ets-sta10#40](https://github.com/opengeospatial/ets-sta10/issues/40), "Test suite assumes IDs are long", is open. | ETS defect. The paths these tests walk are covered by `crates/antares-sta/tests/sta/navigation.rs`. Numeric ids would make URNs unstable across views and are not planned. |
| Level 2, all 8 | SKIP (setup fails) | Create-Update-Delete: the setup reads the `@iot.id` of an Observation as a long, and every test POSTs, PATCHes or DELETEs. The endpoint is read only (`405` with `Allow`); writes go through NGSI-LD under the Endpoint's Policy. | Not supported, by design (sta-mapping section 4). |
| Level 3, all 8 | SKIP (setup fails) | The setup creates its own Things, Datastreams and Observations with POST and reads their ids from the `Location` header (`begin 0, end -1`): a read-only endpoint answers `405`. The query options themselves are served and tested in `crates/antares-sta/tests/sta/sets.rs` and `bounds.rs`. | Not reachable while the endpoint is read only. |

## 5. The gate goes red

A deliberate break proved the gate locally: dropping the Location's
`HistoricalLocations@iot.navigationLink` again and rerunning the pinned
image made `baseline.py check` exit 1 on
`Conformance Level 1 :: Capability1Tests.readEntitiesAndCheckResponse`.

## Related

- [SensorThings API endpoint of a view](sta.md)
- [OGC API - Features compliance](ogc-compliance.md)
- [OData compliance](odata-compliance.md)
