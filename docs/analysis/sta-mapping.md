# OGC SensorThings API v1.1 (Sensing, read only) surface: Mapping onto `ViewOps` (T-4462)

Sources: OGC SensorThings API Part 1: Sensing v1.1 (OGC 18-088), ISO 19156:2023 (OMS), Antares `antares-ops`
and `antares-ogc`, legacy `context-gateway` (`translators/sta.rs`, `handlers/sta.rs`).
The surface mounts at `/x/views/{view}/sta/v1.1`.

## 1. Entity Set Mapping

| STA Entity Set | ISO 19156 Concept | NGSI-LD Source | ID Form | OperationId |
|---|---|---|---|---|
| `Things` | Host / Platform | Entity of shown type | `{urn}` | `QueryEntities`, `RetrieveEntity` |
| `Locations` | Location | Entity's `location` GeoProperty | `{urn}/location` | `QueryEntities`, `RetrieveEntity` |
| `HistoricalLocations` | Trajectory step | Temporal history of `location` | `{urn}/historicalLocation/{time}` | `TemporalQuery`, `TemporalRetrieve` |
| `Datastreams` | ObservationGroup | Numeric Property attribute | `{urn}/{attr}` or `{urn}/{attr}#{datasetId}` | `QueryEntities`, `RetrieveEntity` |
| `Sensors` | Observer | Referenced SDM `Device` via `refDevice` | `{deviceUrn}` | `RetrieveEntity` |
| `Observations` | Observation | Measurement instance | `{dsId}/{phenomenonTime}` | `TemporalRetrieve`, `QueryEntities` |
| `ObservedProperties` | ObservableProperty | Numeric Property definition | Expanded IRI from `@context` | `QueryEntities`, `RetrieveEntity` |
| `FeaturesOfInterest` | FeatureOfInterest | `refPointOfInterest` or entity `location` | Target URN or `{urn}/location` | `RetrieveEntity`, `QueryEntities` |

## 2. Query Options

| Option | Syntax / Bounds | Mapping / Handling |
|---|---|---|
| `$filter` | Max 4 096 bytes, comparisons + `and`/`or` | Compiled to NGSI-LD `q` (`==`, `!=`, `<`, `>`, `<=`, `>=`, `;`, `\|`) |
| `$top` | 1 to 100 (default 100) | Upstream `limit` |
| `$skip` | Whole number $\ge 0$ | Upstream `offset` / slice |
| `$count` | `true` or `false` | Yields `@iot.count` |
| `$expand` | Max depth 2 (`Locations`, `Datastreams`) | Inline navigation objects |
| `$orderby` | `phenomenonTime desc` | Sorts observation series |
| `$select` | Property names | Projection |

## 3. Seven Mapping Rules & Differences against Legacy Gateway

1. **ObservedProperty Identity:**
   - *Legacy:* Local URL fragment `{endpoint}/schema/index.json#{name}`.
   - *Antares:* Expanded IRI from `@context` (`antares_ops::core_iri`). Ensures NO2 across views/cities resolves to the exact same ObservedProperty identity.
2. **Multi-instance Attributes:**
   - *Legacy:* Did not differentiate datasetId instances.
   - *Antares:* Default instance is `{urn}/{attr}`; datasetId instance is `{urn}/{attr}#{datasetId}`.
3. **Sensors:**
   - *Legacy:* Empty set.
   - *Antares:* Derived from `refDevice` relationship to SDM Device entities. Absence of device means absence of sensor.
4. **FeaturesOfInterest:**
   - *Legacy:* Empty set.
   - *Antares:* Derived from `refPointOfInterest` or entity `location`.
5. **HistoricalLocations:**
   - *Legacy:* Empty set.
   - *Antares:* Derived from temporal query on `location` for moving things.
6. **Datastreams & Units:**
   - Numeric properties only; unit of measurement definitions use `https://vocabulary.uncefact.org/UnitMeasureCode#{unitCode}`.
7. **Honest Absence:**
   - If an attribute or related entity does not exist or is hidden by the view, it is omitted. No placeholder objects.

## 4. Limits and Bounds

- `$filter` length is bounded at 4 096 bytes.
- Character slicing respects UTF-8 multibyte character boundaries (T-4323).
- Pages are cut at 1 MiB (`MAX_PAGE_BYTES`) or `$top`, whichever is reached first; cut pages carry `@iot.nextLink`.
- Writes, MQTT, Tasking (Part 2), and MultiDatastream are out of scope.
