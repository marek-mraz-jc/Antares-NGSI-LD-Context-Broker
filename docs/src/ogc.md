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
GET /x/views/{view}/ogc/features/collections/{collectionId}/items
GET /x/views/{view}/ogc/features/collections/{collectionId}/items/{featureId}
```

Mount it with `ANTARES_API_SURFACES=admin,views,ogc`. The design is
ADR-0029 and `docs/analysis/ogc-features-mapping.md`; this page is what
the build serves today (T-3744): Part 1 Core, `geojson` and `oas30`.
`HEAD` answers like `GET` without the body.

## 1. Collections

The collections are the view's Tenant's LinkML schema
(`/q/schemas/{tenant}`) intersected with what the view shows:

- one collection per class whose type the view shows, its `collectionId`
  the class name (a name that is no plain path segment is left out);
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

Any other parameter, a repeated one, or a parameter on a resource that
takes none is `400`. `/items/{featureId}` is one Query Entities by `id`
and type: an Entity the view hides, of another type, absent, or an id
that is no URI is `404`.

## 3. Refusals and errors

A method other than `GET` or `HEAD` is `405`, an `Accept` without JSON is
`406`. An error is `application/problem+json` (RFC 7807) with a fixed
`detail`: it names no hidden type or Attribute and echoes no request
value. A view that does not allow `query_entities` refuses `/items` with
the status the view gives (`405`).

## Related

- [Endpoint views](views.md)
- [OData endpoint of a view](odata.md)
- `docs/analysis/ogc-features-mapping.md` (rows M, C, R, F, W, S)
- ADR-0029 (the OGC surface and the `ogc-cql2` crate)
