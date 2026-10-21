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

Every `@iot.navigationLink` a resource carries answers (SensorThings 1.1
`req/resource-path`, T-4878): a Thing's `Locations`, `Datastreams` and
`HistoricalLocations`; a Location's `Things` and `HistoricalLocations`; a
HistoricalLocation's `Thing` and `Locations`; a Datastream's `Thing`,
`Sensor`, `ObservedProperty` and `Observations`; an Observation's
`Datastream` and `FeatureOfInterest`; a Sensor's and an ObservedProperty's
`Datastreams`; a FeatureOfInterest's `Observations`. A Datastream carries
`Sensor` only when its Entity has a `refDevice`, and that link is `404`
when the view hides the Device. A member path such as `Things('{urn}')/name`
answers `{"name": …}`. `resultTime` is on every Observation, `null` when
the instance has no `modifiedAt`. A key that is no URI, such as `Sensors(1)`,
answers `404`.

## 1. What a set holds

Every set reads the view's Query Entities, naming the types the view shows,
so a hidden type or Attribute is in no set, no count and no id, and a
hidden id answers the `404` an unknown one does.

| Set | From | Id |
|---|---|---|
| `Things` | each Entity | its URN |
| `Locations` | its `location` | `{urn}/location` |
| `HistoricalLocations` | the timed instances of its `location` history | `{urn}/historicalLocation/{observedAt}` |
| `Datastreams` | each instance of a numeric Property | `{urn}/{attribute}`, `#{datasetId}` after it for a dataset |
| `Observations` | the current instance; a Datastream's are its history | `{datastream id}/{phenomenonTime}` |
| `ObservedProperties` | each measured Attribute | its expanded IRI |
| `Sensors` | the `Device` its `refDevice` points at | the Device's URN |
| `FeaturesOfInterest` | the target of `refPointOfInterest`, else its `location` | the target's URN, else `{urn}/location` |

## 2. Query options

| Option | Behaviour |
|---|---|
| `$top`, `$skip` | resources per page, at most 100 (default 100); `$skip` at most 10 000; on `Datastreams`, `Observations` and `ObservedProperties` a `$skip` reaching past the first 1 000 Entities is `400`: follow `@iot.nextLink` |
| `$count=true` | `@iot.count`: the Query Entities count on `Things`, and on any other set only when the page is the whole answer |
| `$filter` | comparisons, `and`, `or`, parentheses and `substringof` as the NGSI-LD `q`; on a Datastream's Observations `phenomenonTime` predicates on the top-level `and` become the temporal window; `id eq` and `id ne` with a quoted id on the top-level `and` select by id (`id eq` beside another comparison, or `id` elsewhere, is `400`); at most 4 096 bytes; anything else `400` naming it |
| `$expand` | `Locations` and `Datastreams` inline on a Thing, the current `Observations` on a Datastream; a path deeper than 2 is `400` |
| `$select` | the members each resource keeps |
| `$orderby` | `phenomenonTime desc` on a Datastream's Observations |

An Observation's `phenomenonTime` is the instance's `observedAt`, else the
time its value was last set (`modifiedAt`); an instance with neither is no
Observation. A page is cut at 1 MiB between resources, and a cut or a full
page carries `@iot.nextLink` with the request's own options. Errors
are the OData JSON error body with a fixed message.

## Related

- [Surfaces](surfaces.md)
- [Endpoint views](views.md)
- [OGC API - Features endpoint of a view](ogc.md)
- [SensorThings API compliance](sta-compliance.md)
