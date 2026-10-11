# OData endpoint of a view

An `odata` build (`cargo build -p antares-broker --features odata`) serves
every loaded [view](views.md) to OData 4.0 and 4.01 clients:

```text
GET  /x/views/{view}/odata/v4/
GET  /x/views/{view}/odata/v4/$metadata
GET  /x/views/{view}/odata/v4/{EntitySet}
GET  /x/views/{view}/odata/v4/{EntitySet}('{id}')
GET  /x/views/{view}/odata/v4/{EntitySet}/$count
POST /x/views/{view}/odata/v4/{EntitySet}/$query
```

Mount it with `ANTARES_API_SURFACES=admin,views,odata`. The design is
`docs/analysis/odata-linkml-mapping.md` (T-3441); this page is what the
build serves today (T-3508). Writes and `$batch` come with T-3509.

## 1. The model

The EDM model is the view's Tenant's LinkML schema (`/q/schemas/{tenant}`)
intersected with what the view shows:

- one EntitySet per class the view's `selector` holds, named after the
  class; one property per slot the view's rules show;
- a Relationship to a type the view shows is a navigation property; to
  any other type, the object URI as a string (R5);
- every type derives from `NgsiLd.Entity`: `id` (the key), `_types`,
  `scope`, `createdAt`, `modifiedAt`.

A view without a schema, or with one the build cannot read, has no OData
endpoint (`404`). Nothing the view hides appears in a document, a name or
an error: a hidden type or Attribute and an unknown one answer alike.

The model is built once per view, shown names, schema revision and OData
version, and cached (at most 256 models). `$metadata` carries a strong
`ETag`, answers `If-None-Match` with `304` and is
`Cache-Control: private, no-cache`.

## 2. Reads

Each read is one NGSI-LD operation of the view, run through `antares-ops`,
so the policy gate fires and the view narrows the answer as on the view
root.

| OData | NGSI-LD |
|---|---|
| `{Set}` | Query Entities (`type`), `count=true` |
| `{Set}('{id}')`, `{Set}(id='{id}')` | Retrieve Entity |
| `{Set}/$count` | Query Entities, `limit=0&count=true`, `text/plain` |
| `POST {Set}/$query` | the same `GET`: the body's options (`text/plain` or a form) joined to the URL's |
| `$filter` | `q` and `id` (section 3) |
| `$orderby` | `orderBy` on `id` and single-valued values, `;desc` |
| `$select` | `attrs`; `createdAt`/`modifiedAt` add `options=sysAttrs` |
| `$expand` | `join=inline&joinLevel=1` (section 4) |
| `$top`, `$skip`, `Prefer: odata.maxpagesize` | `limit`, `offset`; a page is at most 1000, 100 by default |
| `$count=true` | `@odata.count` from `NGSILD-Results-Count` |

A page that leaves rows carries `@odata.nextLink` with a `$skiptoken`. The
token is bound to the view and the query: a token beside `$skip`/`$top`,
from another query or another view is `400`. A query in a Tenant that holds
nothing yet answers an empty set.

## 3. `$filter`

A filter is translated to `q`, never evaluated in the surface. Each
Attribute is named by a term `t{n}` the request's own `@context` defines,
so a client `Link` header cannot change what a filter means.

- `eq`, `ne`, `gt`, `ge`, `lt`, `le`, `in`, `and`, `or`, `not` around the
  string functions, `eq null` and `ne null`;
- `contains`, `startswith`, `endswith` on `Edm.String` members, escaped
  and anchored (`~=`); a `"` in the pattern is matched as `\x22`;
- `id eq '…'` or `id in (…)` as a top-level conjunct (`id=`);
- one single-valued navigation hop, `owner/name eq 'Ann'`, as
  `t0{t2:t1}=="Ann"` under a `join`: the broker resolves the target
  through the view and the engine, a hidden one like a dangling link.

`ne` holds on an absent member, as in OData. A number past 2^53 is `400`
(NGSI-LD compares `f64`). A filter longer than 4096 bytes as `q` is `400`
`TooComplexQuery`. Lambda operators, arithmetic, other functions, two
members compared and paths past one hop are `501`.

## 4. `$expand`

`$expand=owner`, `*`, `owner/$ref` and a nested `$select` are served one
level deep. The join names only the expanded Relationships, so a
navigation that is not expanded costs nothing. A target the view or the
engine hides, or one of another type, reads like an absent one: `null` for
a single navigation, left out of a collection. `/$ref` writes the
Relationship's object as an entity reference. A walk past the broker's
budget of 1000 lookups is `400`, never a partial answer. A nested
`$expand`, `$levels` and the other nested options are `501`.

## 5. Capabilities

`$metadata` annotates each EntitySet with what the reads serve:
`Countable`, `Filterable` and `Sortable` with the members they refuse
(`NonFilterableProperties`, `NonSortableProperties`), `Expandable` one
level, no insert, update, delete, upsert, deep insert or search, and
`QuerySegmentSupported`. The annotations are advisory: every request is
still gated, and a read the engine denies is `403` whatever they say.

## 6. Refusals and errors

The OData features no NGSI-LD operation stands behind (matrix 7.1 of
`docs/analysis/operation-matrix.yaml`: `$apply`, `$search`, `$compute`,
actions, functions, `$crossjoin`, `$all`, delta, media, and the rest) are
refused with the status the matrix names, before any gate or store access.
A custom query option is `400`; a write method is `405` (`501` where a
later round maps it).

An error is `{"error": {"code", "message"}}` with a fixed message: it
names no hidden type or Attribute, echoes no literal of the request and
carries no NGSI-LD `ProblemDetails`.

## Related

- [Endpoint views](views.md)
- [MCP endpoint of a view](mcp.md)
- `docs/analysis/odata-linkml-mapping.md`, `docs/analysis/surfaces-threat-model.md` (O1 to O26)
- ADR-0024 (the OData crates)
