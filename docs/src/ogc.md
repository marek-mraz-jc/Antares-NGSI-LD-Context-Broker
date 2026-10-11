# OGC API - Features endpoint of a view

An `ogc` build (`cargo build -p antares-broker --features ogc`) serves
every loaded [view](views.md) to OGC API - Features clients (QGIS, GDAL,
OpenLayers, the OGC test suite):

```text
GET /x/views/{view}/ogc/features/
GET /x/views/{view}/ogc/features/conformance
GET /x/views/{view}/ogc/features/api
GET /x/views/{view}/ogc/features/collections
GET /x/views/{view}/ogc/features/collections/{collectionId}
GET /x/views/{view}/ogc/features/collections/{collectionId}/schema
GET /x/views/{view}/ogc/features/collections/{collectionId}/sortables
GET /x/views/{view}/ogc/features/collections/{collectionId}/items
GET /x/views/{view}/ogc/features/collections/{collectionId}/items/{featureId}
POST   /x/views/{view}/ogc/features/collections/{collectionId}/items
PUT    /x/views/{view}/ogc/features/collections/{collectionId}/items/{featureId}
PATCH  /x/views/{view}/ogc/features/collections/{collectionId}/items/{featureId}
DELETE /x/views/{view}/ogc/features/collections/{collectionId}/items/{featureId}
OPTIONS any of them
```

Mount it with `ANTARES_API_SURFACES=admin,views,ogc`. The design is
ADR-0029 and `docs/analysis/ogc-features-mapping.md`; this page is what
the build serves today (T-3744, T-3754, T-3746): Part 1 Core, `geojson`
and `oas30`, Part 2 CRS84 and CRS84h, from the Part 4 draft (OGC
20-002r1) create, replace, update and delete, and from the Part 5 draft
(OGC 23-058r1) schemas, core roles, sortables and feature references.
`HEAD` answers like `GET` without the body.

## 1. Collections

The collections are the view's Tenant's LinkML schema
(`/q/schemas/{tenant}`) intersected with what the view shows:

- one collection per class whose type the view shows, its `collectionId`
  the class name (a name that is no plain path segment is left out); a
  view of `*` (ADR-0023 rule 12) shows every class with every slot;
- a Feature's `geometry` is the slot annotated `ogc_role:
  primary-geometry`, else `location` when the view shows it, else `null`;
- `properties` are the other Attributes the view shows, by slot name;
- a slot annotated `ogc_role: primary-instant`, or
  `primary-interval-start` / `primary-interval-end`, is the time
  `datetime` filters on.

A view without a schema, or with one the build cannot read, has no
endpoint (`404`). The collections carry no `extent`, and the
`FeatureCollection` no `timeStamp` (both optional in Part 1).

## 2. Items

`/items` is one Query Entities of the collection's type through the view
(`format=simplified`, `count=true`), so the view's `q` and the policy
engine narrow Features, `numberMatched` and the next link alike.

| Parameter | Meaning |
|---|---|
| `limit` | 1 to 10 000, default 10; above 1 000 it is clamped to 1 000 |
| `bbox` | `minx,miny,maxx,maxy` or the 6-number form, CRS84; a box with `minx > maxx` crosses the antimeridian. A `geoQ` `intersects` on the geometry; Features without a geometry are kept (Part 1 Req 24 C), served after the ones inside |
| `datetime` | an RFC 3339 instant or `start/end`, `..` or empty for an open end; both ends open is `400`. Features without a time are kept |
| `next` | the token of a `next` link; it is bound to the view, the collection and the query, any other use is `400` |
| `sortby` | `[+\|-]name` by commas, names from `/sortables`; an NGSI-LD `orderBy`. With `bbox`, the Features without a geometry follow the sorted ones |
| `properties` | property names by commas, empty for none; an NGSI-LD `pick` that keeps `id`, `type` and the geometry |
| `profile` | `rel-as-key` (default), `rel-as-uri` or `rel-as-link`, below |
| `crs`, `bbox-crs` | CRS84 or CRS84h (the URI or `[OGC:CRS84]`); any other is `400` naming both. The answer carries `Content-Crs` |

Any other parameter, a repeated one, or a parameter on a resource that
takes none is `400`; a property the view hides is refused like a
misspelt one. `/items/{featureId}` takes `properties`, `profile` and `crs` and
is one Query Entities by `id` and type: an Entity the view hides, of another type, absent, or an id
that is no URI is `404`.

## 3. Schemas and references

`/schema` is a JSON Schema 2020-12 document (`application/schema+json`)
of the Features of a collection: `id` (role `id`), `geometry` (role
`primary-geometry`) and every property the view shows, in Feature order
(`x-ogc-propertySeq`), each with its type, `x-ogc-definition` (the
Attribute IRI), `x-ogc-unit` (the slot's `unit.ucum_code`), its
description, and `x-ogc-role` from the slot's `ogc_role` annotation.
`/sortables` lists the properties `sortby` may name: one scalar value, no
Relationship, no geometry, no array.

A Relationship is a property with role `reference`; when its range names a
class the view shows, `x-ogc-collectionId` names that collection and the
profile decides how a Feature writes it: the URN (`rel-as-key`), the
target Feature's URI (`rel-as-uri`) or `{"href", "title"}`
(`rel-as-link`). A Relationship to a class the view does not show stays its
URN in every profile.

## 4. Writes

A view that offers `create_entity`, `replace_entity`, `merge_entity` or
`delete_entity` takes `POST /items`, `PUT`, `PATCH` and `DELETE
/items/{featureId}`, each one Create, Replace, Merge or Delete Entity
through the view, so the view's rules and the Policy decide it. `OPTIONS`
and a `405` list in `Allow` the methods the view offers on the resource;
`/api` documents exactly those.

- `POST` and `PUT` take a GeoJSON Feature (`application/geo+json` or
  `application/json`), `PATCH` an RFC 7396 merge patch
  (`application/merge-patch+json`, a `null` deletes); anything else is
  `415`, a body above 1 MiB `413`.
- The geometry becomes the collection's GeoProperty, each property an
  Attribute of its LinkML kind; a property the view hides is refused like
  a misspelt one (`400`).
- `POST` uses the Feature's `id` when it has one, else mints
  `urn:ngsi-ld:{Type}:{uuid}`, and answers `201` with `Location`; the
  others answer `204`. An absent or hidden Feature is `404`.
- `If-Match` and `If-Unmodified-Since` are `501`: the broker has no
  atomic conditional write, and ignoring one could lose an update. A
  `Content-Crs` other than CRS84 or CRS84h is `400`.

## 5. Refusals and errors

A method the view does not offer on a resource is `405` with `Allow`, a
read whose `Accept` takes no JSON `406`. An error is `application/problem+json` (RFC 7807) with a fixed
`detail`: it names no hidden type or Attribute and echoes no request
value. A view that does not allow `query_entities` refuses `/items` with
the status the view gives (`405`).

## Related

- [Endpoint views](views.md)
- [OData endpoint of a view](odata.md)
- `docs/analysis/ogc-features-mapping.md` (rows M, C, R, F, W, S)
- ADR-0029 (the OGC surface and the `ogc-cql2` crate)
