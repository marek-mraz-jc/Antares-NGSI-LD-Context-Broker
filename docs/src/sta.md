# SensorThings API endpoint of a view

An `sta` build (`cargo build -p antares-broker --features sta`) serves every
loaded [view](views.md) to OGC SensorThings API v1.1 clients, Sensing
profile, read only:

```text
GET /x/views/{view}/sta/v1.1/
GET /x/views/{view}/sta/v1.1/{Set}
GET /x/views/{view}/sta/v1.1/{Set}('{id}')
GET /x/views/{view}/sta/v1.1/{Set}('{id}')/{navigation}
GET /x/views/{view}/sta/v1.1/{Set}('{id}')/{member}
OPTIONS any of them
```

Mount it with `ANTARES_API_SURFACES=admin,views,sta`. The sets are
`Things`, `Locations`, `HistoricalLocations`, `Datastreams`, `Sensors`,
`Observations`, `ObservedProperties` and `FeaturesOfInterest`; the mapping
and its reasons are `docs/analysis/sta-mapping.md` (T-4462). `HEAD` answers
like `GET` without the body; every other method is `405` with `Allow`.

The `{id}` of a link is the id as an OData key literal (a quote written
twice), percent-encoded but for the unreserved characters, `:` and the
quote: `Datastreams('urn:ngsi-ld:Car:1%2FNO2%23urn:ngsi-ld:Dataset:a')`.
Section 3 defines the id itself.
Every `@iot.selfLink`, `@iot.navigationLink` and `@iot.nextLink` is written
so; a request may send the key encoded or not, as the path is decoded once
before the key is read.

Every `@iot.navigationLink` a resource carries answers (SensorThings 1.1
`req/resource-path`, T-4878): a Thing's `Locations`, `Datastreams` and
`HistoricalLocations`; a Location's `Things` and `HistoricalLocations`; a
HistoricalLocation's `Thing` and `Locations`; a Datastream's `Thing`,
`Sensor`, `ObservedProperty` and `Observations`; an Observation's
`Datastream` and `FeatureOfInterest`; a Sensor's and an ObservedProperty's
`Datastreams`; a FeatureOfInterest's `Observations`. Every Datastream has
exactly one Sensor: the Device its Entity's `refDevice` names (`404` when
the view hides that Device), else the Sensor `unknown`, whose Datastreams
are those of every Entity naming no Device (T-5205). A member path such as
`Things('{urn}')/name` answers `{"name": …}`. A key that is no URI, such as
`Sensors(1)`, answers `404`.

## 1. What a set holds

Every set reads the view's Query Entities, naming the types the view shows,
so a hidden type or Attribute is in no set, no count and no id, and a
hidden id answers the `404` an unknown one does.

| Set | From | Id |
|---|---|---|
| `Things` | each Entity | `E` |
| `Locations` | its `location` now; a HistoricalLocation's, its `location` then | `{E}/location`; `{E}/location/{T}` |
| `HistoricalLocations` | the timed instances of its `location` history | `{E}/historicalLocation/{T}` |
| `Datastreams` | each instance of a numeric Property | `{E}/{A}`, `#{D}` after it for a dataset |
| `Observations` | each numeric instance: the current one, and on a Datastream its history | `{datastream id}/{T}` |
| `ObservedProperties` | each measured Attribute | its expanded IRI |
| `Sensors` | the `Device` its `refDevice` points at; `unknown` first | the Device's id; `unknown` |
| `FeaturesOfInterest` | the target of `refPointOfInterest`, else its `location` | `{P}`, else `{E}/location` |

A Datastream's `unitOfMeasurement` is always present: from the instance's
`unitCode` (`name` the UN/CEFACT code, `symbol` its UCUM symbol for the
common codes, else the code, `definition` the code's IRI), else every member
`null`. Its `observationType` is `OM_CountObservation` for a whole number
without a unit, else `OM_Measurement`. A Sensor's `encodingType` is
`application/json`: its `metadata` is the Device's `controlledProperty` as
JSON, not a SensorML or PDF document.

## 2. Query options

| Option | Behaviour |
|---|---|
| `$top`, `$skip` | resources per page, at most 100 (default 100); `$top=0` answers `@iot.count` alone; `$skip` at most 10 000; on `Datastreams`, `Observations` and `ObservedProperties` a `$skip` reaching past the first 1 000 Entities is `400`: follow `@iot.nextLink`. A page of those sets reads past Entities that hold none of its resources, up to 1 000 Entities per request |
| `$count=true` | `@iot.count`: the Query Entities count on `Things`, and on any other set only when the page is the whole answer |
| `$filter` | see below; at most 4 096 bytes; anything else `400` naming it |
| `$expand` | `Locations`, `Datastreams` and `Datastreams/Observations` on a Thing, `Observations` on a Datastream; an inlined `Observations` is the current Observation, so its nested options are not read; any other path is `400` naming it |
| `$select` | the members each resource keeps, on a list and on one resource; `id` is `@iot.id` |
| `$orderby` | `phenomenonTime desc` alone, on a Datastream's Observations; any other order is `400` |

`$filter` takes comparisons (`eq`, `ne`, `gt`, `ge`, `lt`, `le`), `and`,
`or`, parentheses and `substringof('text', property)`, its text matched
literally. A text literal may hold spaces, parentheses and a doubled quote.
The properties are SensorThings names, each read as the NGSI-LD Attribute
of the Entity behind the resource:

| Set | Property | NGSI-LD |
|---|---|---|
| `Things` | `name`, `description`, `properties/{a}` | the Attribute `name`, `description`, `{a}` |
| `Datastreams` | `Thing/name`, `Thing/description`, `Thing/properties/{a}` | the same, of the Datastream's Entity |
| `Observations` | `Datastream/Thing/…` | the same, of the Observation's Entity |
| `Things`, `Datastreams`, `Observations` | `properties/type eq '{type}'` (with the prefix above) on the top-level `and` | the view's `type` selector, narrowed to that type |
| every set | `id eq '{id}'`, `id ne '{id}'` on the top-level `and` | read by id |

A Datastream's Observations take `phenomenonTime` predicates on the
top-level `and`, as the temporal window: `ge t` holds `t`, `gt t` starts one
millisecond after it, `lt t` ends before it, `le t` one millisecond after
it, and `eq t` is the millisecond at `t`. Every other property, `result`
included, is `400` naming it.

A Datastream's Observations are newest first: its Entity's current instance
and the newest `$skip + $top + 1` instances of its history, read with their
system times, so a page never repeats an earlier one. An Entity with no
temporal record lists its current Observation.

An Observation's `phenomenonTime` is the instance's `observedAt`, and so is
its `resultTime`. An instance without one takes the time its value was last
set (`modifiedAt`) as `phenomenonTime`, has `resultTime: null` and
`parameters: {"timeSource": "modifiedAt"}`; an instance with neither time is
no Observation. A text and the deletion marker `urn:ngsi-ld:null` are no
Observation. A page is cut at 1 MiB between resources, and a cut or a full
page carries `@iot.nextLink` with the request's own options; an empty page
carries none. Errors are the OData JSON error body with a fixed message.

## 3. Resource ids

The ids are a contract, version 1, pinned by the test
`the_ids_of_a_fixture_are_pinned` in `crates/antares-sta/src/mapping.rs`: a
change to it is a new version, listed here, since a client may have stored
an id. The parts of an id are:

- `E`: the Entity id, `P`: a point of interest's Entity id, `A`: the
  Attribute name as the view answers it under the core @context (a term the
  core @context defines, else the expanded IRI), `D`: a `datasetId`. Each
  enters an id with `%`, `/` and `#` percent-encoded (`%25`, `%2F`, `%23`),
  so `https://example.org/car/9` is `https:%2F%2Fexample.org%2Fcar%2F9`.
- `T`: an instant in UTC with three fractional digits,
  `2026-01-01T00:00:00.000Z`; a time stored with an offset is kept as
  stored.

A Thing's id is `E` itself. Two vocabularies' `NO2` are two Datastreams,
since `A` keeps the whole name. An ObservedProperty's id is the expanded
IRI of its Attribute and changes when the @context does.

## 4. Not served

The review of 2026-10-22 (T-5205) listed these; each answers as written
here:

- One Location per `location` instance: a multi-instance `location`,
  `observationSpace`, `operationSpace` and other GeoProperties are no
  Location.
- `@iot.count` on a set other than `Things` past the first page, or on a
  first page that is not the whole answer.
- `result` and every other non-time property in a Datastream's Observations
  `$filter`: Retrieve Temporal Evolution takes no `q`.
- A neutral observation model shared with a SensorThings 2.0 encoding: the
  `/sta/v1.1` path leaves room for `/sta/v2.0` beside it, and the shared
  representation contract of T-5207 decides the model first.
- The selection of which numeric Attributes are measurements: every numeric
  Property is a Datastream; a view hides the ones it does not publish.

## Related

- [Surfaces](surfaces.md)
- [Endpoint views](views.md)
- [OGC API - Features endpoint of a view](ogc.md)
- [SensorThings API compliance](sta-compliance.md)
