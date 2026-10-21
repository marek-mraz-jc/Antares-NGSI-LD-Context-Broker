# OGC SensorThings API v1.1 (Sensing, read only) surface: mapping onto `ViewOps` (T-4462)

Sources: OGC SensorThings API Part 1: Sensing v1.1 (OGC 18-088), ISO 19156:2023 (OMS), Antares `antares-ops`
and `antares-ogc`, legacy `context-gateway` (`translators/sta.rs`, `handlers/sta.rs`). The crate is
`crates/antares-sta`; it mounts at `/x/views/{view}/sta/v1.1` (feature and surface name `sta`). The user page is
`docs/src/sta.md`.

## 1. Entity sets

Every read is an operation of the view. Query Entities always names the types the view shows (`type=`, NGSI-LD
5.7.2 takes no query without one); a view of every type reads `local=true`. Entities are read under the core
@context.

| STA set | ISO 19156 concept | NGSI-LD source | Id | Operation |
|---|---|---|---|---|
| `Things` | Host | the Entity | `{urn}` | Query Entities (`id=` for one) |
| `Locations` | the Host's position | its `location` GeoProperty | `{urn}/location` | Query Entities |
| `HistoricalLocations` | a step of the Host's trajectory | each `location` instance with an `observedAt` | `{urn}/historicalLocation/{observedAt}` | Query (set) and Retrieve (one, a Thing's) Temporal Evolution |
| `Datastreams` | ObservationGroup | each instance of a numeric Property | `{urn}/{attribute}`, `{urn}/{attribute}#{datasetId}` | Query Entities |
| `Sensors` | Observer | the `Device` the Entity's `refDevice` points at | the Device's URN | Query Entities by `id` |
| `Observations` | Observation | the current instance; a Datastream's: its history | `{datastream id}/{phenomenonTime}` | Query Entities; Retrieve Temporal Evolution (`lastN`) |
| `ObservedProperties` | ObservableProperty | each measured Attribute | its expanded IRI | Query Entities (`attrs=` for one) |
| `FeaturesOfInterest` | FeatureOfInterest | the target of `refPointOfInterest`, else the Entity's `location` | the target's URN, else `{urn}/location` | Query Entities by `id` |

`{attribute}` is the Attribute's local name: the last segment of the IRI the answer names it by, which is the
term a Smart Data Model and the gateway use. A Datastream's Observations read the Attribute under the name the
view shows whose local name it is.

## 2. Query options

| Option | Bounds | Handling |
|---|---|---|
| `$filter` | at most 4 096 bytes; split on character boundaries only (T-4323) | comparisons, `and`, `or`, parentheses, `substringof` compiled to `q` (the gateway's subset); `not` and any other operator a `400` naming it; on a Datastream's Observations `phenomenonTime` predicates on the top-level `and` become `timerel`/`timeAt`/`endTimeAt` |
| `$top` | 1 to 100, default 100 | `limit` of the Query Entities; on a Thing's list and on Observations of a Datastream, resources |
| `$skip` | 0 to 10 000 | `offset`; on Observations of a Datastream it widens `lastN` |
| `$count` | `true` | `@iot.count`: on `Things` the Query Entities count; on another set only when the page is the whole answer, since an Entity holds several of its resources and the count of those is in no answer |
| `$expand` | depth at most 2 | `Locations`, `Datastreams` inline on a Thing |
| `$select` | names | the members each resource keeps |
| `$orderby` | `phenomenonTime desc` | newest first on Observations of a Datastream; any other ordering leaves the order as read |

A page is cut at 1 MiB between Entities, never inside one; a cut page or a full one carries `@iot.nextLink`, the
request's options with the next `$skip`. A resource id appears once per page.

## 3. The seven rules and what changed against the gateway

1. **ObservedProperty**: id and `definition` are the Attribute's expanded IRI. The gateway made
   `{urn}/{attribute}` with a definition under the endpoint's own schema, so NO2 at two Entities, views or cities
   were two ObservedProperties; now they are one.
2. **Multi-instance Attributes**: the default instance stays `{urn}/{attribute}`; an instance with a `datasetId`
   is `{urn}/{attribute}#{datasetId}`, and its Observations read only the instances of that dataset. The gateway
   folded every instance into one Datastream.
3. **Sensors**: an Entity of type `Device` (Smart Data Models) that a shown Entity's `refDevice` points at,
   read through the view; `metadata` is its `controlledProperty`. No `refDevice`, or a Device the view hides: no
   Sensor. The gateway answered an empty set.
4. **FeaturesOfInterest**: the target of `refPointOfInterest` with its own URN and location when the view shows
   it, otherwise the Entity's own `location` (`{urn}/location`, the Location's id). An Entity pointing at a hidden
   target has none. A FeatureOfInterest by a URN is that Entity with a location. The gateway answered an empty set.
5. **HistoricalLocations**: the instances of the `location` history with an `observedAt` (moving things: a
   Vehicle). The gateway answered an empty set.
6. **Datastreams and units**: one per instance of a Property whose value is a number;
   `unitOfMeasurement.definition` is `https://vocabulary.uncefact.org/UnitMeasureCode#{unitCode}`. Without a
   `unitCode` the unit is absent, where the gateway answered a unit named `unknown`.
7. **What is absent stays absent** (EP-13): a non-numeric Attribute is no Datastream, a hidden Attribute is in no
   set and its Datastream's Observations are the unknown `404`; no placeholder objects.

## 4. Limits

- A HistoricalLocations page reads the last 100 moves of each Entity; the history of one Thing pages through its
  own list. The broker's temporal Query pages its candidates before the time filter (a short page is not the end),
  so the set reads a full page as "more" and stops at a short one.
- An Observation by id is the current instance, else one of the last 10 000 of its Datastream's history (T-4878).
- `$filter` applies to a set and to a Datastream's Observations; the other navigation collections (a Thing's or a
  Location's lists, and since T-4878 a Sensor's or an ObservedProperty's Datastreams and a FeatureOfInterest's
  Observations) page with `$top` and `$skip` and ignore it.
- `{member}/$value` is not served.
- A Thing has one Location, `{urn}/location`, its `location` Attribute: a HistoricalLocation's `Locations` and a
  Location's `HistoricalLocations` are that one Location and every move of that Attribute.
- `@iot.count` on a set other than `Things` is omitted past the first page.
- Out of scope: Tasking (Part 2), MultiDatastream, writes, MQTT.
