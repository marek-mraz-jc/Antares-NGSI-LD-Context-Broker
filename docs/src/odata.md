# OData endpoint of a view

An `odata` build (`cargo build -p antares-broker --features odata`) serves
every loaded [view](views.md) to OData 4.0 and 4.01 clients:

```text
GET  /x/views/{view}/odata/v4/
GET  /x/views/{view}/odata/v4/$metadata
GET  /x/views/{view}/odata/v4/{EntitySet}
GET  /x/views/{view}/odata/v4/{EntitySet}('{id}')
GET  /x/views/{view}/odata/v4/{EntitySet}/$count
GET  /x/views/{view}/odata/v4/{EntitySet}('{id}')/{property or navigation}
POST /x/views/{view}/odata/v4/{EntitySet}/$query
POST   /x/views/{view}/odata/v4/{EntitySet}
PATCH  /x/views/{view}/odata/v4/{EntitySet}('{id}')
PUT    /x/views/{view}/odata/v4/{EntitySet}('{id}')
DELETE /x/views/{view}/odata/v4/{EntitySet}('{id}')
PUT|PATCH|DELETE /x/views/{view}/odata/v4/{EntitySet}('{id}')/{property}
PUT|DELETE       /x/views/{view}/odata/v4/{EntitySet}('{id}')/{navigation}/$ref
POST   /x/views/{view}/odata/v4/$batch
```

Mount it with `ANTARES_API_SURFACES=admin,views,odata`. The design is
`docs/analysis/odata-linkml-mapping.md` (T-3441); this page is what the
build serves today (T-3508, T-3509).

## 1. The model

The EDM model is the view's Tenant's LinkML schema (`/q/schemas/{tenant}`)
intersected with what the view shows:

- one EntitySet per class the view's `selector` holds, named after the
  class; one property per slot the view's rules show;
- a Relationship to a type the view shows is a navigation property; to
  any other type, the object URI as a string (R5);
- every type derives from `NgsiLd.Entity`: `id` (the key), `_types`,
  `scope`, `createdAt`, `modifiedAt`;
- a class whose LinkML schema marks a slot `identifier: true`, and whose
  view shows it, is keyed by that slot instead (T-3720): the type declares
  the members of `NgsiLd.Entity` itself, and key value `v` names the Entity
  `urn:ngsi-ld:{Class}:{v}`. The key is `Edm.String`, `Edm.Int64` for an
  `integer` slot, or `Edm.Guid` when the slot carries the annotation
  `edm_type: Edm.Guid`, so `Products(2dcd1e3c-e38a-5391-ba63-17ecfeaea3c2)`
  reads `urn:ngsi-ld:Products:2dcd1e3c-e38a-5391-ba63-17ecfeaea3c2`.
  `(ID=v)` names the same Entity. `(id='urn:…')` still names any Entity of
  the set, and it is the canonical key of an Entity whose URN does not
  follow the pattern. A key value never names an Entity of another set.
- the schema narrows and annotates (T-3722): a slot's `edm_type` annotation
  makes an `integer` an `Edm.Byte`, `Edm.SByte`, `Edm.Int16` or `Edm.Int32`,
  a `float` or `double` an `Edm.Single`, and a `string` an `Edm.Guid`,
  `Edm.Duration` or `Edm.Binary`, and a write is checked against that type;
  `description` is `Core.Description`; `odata_computed: "true"` is
  `Core.Computed` and `odata_immutable: "true"` is `Core.Immutable`, and a
  body that writes such a member where the term forbids it is `400`; a
  class's `odata_insertable`, `odata_updatable` or `odata_deletable` set to
  `"false"` takes that write from its set (`405`), never adding one the
  view's `operations` lack.

A view without a schema, or with one the build cannot read, has no OData
endpoint (`404`). Nothing the view hides appears in a document, a name or
an error: a hidden type or Attribute and an unknown one answer alike.

The model is built once per view, shown names, schema revision and OData
version, and cached (at most 256 models). `$metadata` carries a strong
`ETag`, answers `If-None-Match` with `304` and is
`Cache-Control: private, no-cache`. Every JSON answer at `minimal` or
`full` metadata names the tag of the XML document its version reads as
`@odata.metadataEtag` (`@metadataEtag` in 4.01), right after the context
URL, so a client knows when to fetch the model again.

## 2. Reads

Each read is one NGSI-LD operation of the view, run through `antares-ops`,
so the policy gate fires and the view narrows the answer as on the view
root.

| OData | NGSI-LD |
|---|---|
| `{Set}` | Query Entities (`type`), `count=true` |
| `{Set}('{id}')`, `{Set}(id='{id}')`, the identifier's `{Set}(v)` | Retrieve Entity |
| `$entity?$id={url}` | the `{Set}(key)` the percent-decoded URL names, absolute under the service root or relative to it; any other id is `400` |
| `{Set}/$count` | Query Entities, `limit=0&count=true`, `text/plain` |
| `{Set}('{id}')/{prop}`, `.../{prop}/$value` | Retrieve Attribute Value (the view's `operations` name `retrieve_attribute_value`); `$value` is `text/plain` |
| `{Set}('{id}')/{nav}` | Retrieve Entity of the source, then Retrieve Entity of a single target or Query Entities `id=…` of up to 1000 |
| `{Set}('{id}')/{nav}/$ref` | the Relationship's objects as entity references |
| `POST {Set}/$query` | the same `GET`: the body's options (`text/plain` or a form) joined to the URL's |
| `$filter` | `q` and `id` (section 4) |
| `$orderby` | `orderBy` on `id` and single-valued values, `;desc` |
| `$select` | `attrs`; `createdAt`/`modifiedAt` add `options=sysAttrs` |
| `$expand` | `join=inline&joinLevel=1` (section 5) |
| `$top`, `$skip`, `Prefer: odata.maxpagesize` | `limit`, `offset`; a page is at most 1000, 100 by default |
| `$count=true` | `@odata.count` from `NGSILD-Results-Count` |

Below one Entity only `$select` applies; another option there is `501`. A
hidden or absent property or single target is `404`, a hidden target of a
collection navigation is left out, and an operation the view does not offer
is `405` `MethodNotAllowed`.

Context URLs, next links, entity references, `Location` and
`OData-EntityId` are absolute under `ANTARES_PUBLIC_URL`, or `http://{Host}`
of the request when it is unset, as for the JSON-LD context URL.

A page that leaves rows carries `@odata.nextLink` with a `$skiptoken`. The
token is bound to the view and the query: a token beside `$skip`/`$top`,
from another query or another view is `400`. A query in a Tenant that holds
nothing yet answers an empty set.

## 3. Writes

Each write is one NGSI-LD operation of the view, gated like a read, and
the view's rule 7 (pre-image, post-image, allowlist) holds in the handler
it runs. The body is written in the normalized form, so the kind of each
Attribute comes from the model, never from the shape of a value.

| OData | NGSI-LD |
|---|---|
| `POST {Set}` | Create Entity of the set's type, `id` from the body; `201` with the Entity as the view reads it and `Location`, or `204` and `OData-EntityId` under `Prefer: return=minimal` |
| `nav@odata.bind` (4.0), `nav: {"@id": …}` (4.01) | a Relationship; the target is read first with Retrieve Entity under the view |
| `PATCH {Set}('{id}')` | Merge Entity `?type=`: a `null` is the NGSI-LD Null and deletes the Attribute; `204` |
| `PUT {Set}('{id}')` | Replace Entity `?type=`: what the body omits is removed; `204` |
| `DELETE {Set}('{id}')` | Delete Entity `?type=`; `204` |
| `PUT {Set}('{id}')/{prop}` | Replace Attribute `?type=` in the member's kind; `404` when the Entity does not hold it (a `PATCH` of the Entity adds it); `204` |
| `PATCH {Set}('{id}')/{prop}` | Partial Attribute Update `?type=`; `null` is the NGSI-LD Null; `204` |
| `DELETE {Set}('{id}')/{prop}`, `…/{prop}/$value` | Delete Attribute `?type=`; `204` |
| `PUT {Set}('{id}')/{nav}/$ref` | Append Attributes `?type=`: a single-valued navigation as one Relationship, a collection as its full list in its kind (`ListRelationship` keeps the order); every target is read first under the view; `204` |
| `DELETE {Set}('{id}')/{nav}/$ref` | Delete Attribute `?type=`; `204` |

A write the view's `operations` do not offer is `405` before its body is
read. An absent Entity, a hidden one and one of another type are `404`,
and a `PATCH` or `PUT` never creates one (no upsert). A bind to a hidden
target answers the `404` of an absent one and writes nothing. A body key
that differs from the URL's, a computed member (`_types`, `createdAt`,
`modifiedAt`), a Relationship to a type the view hides and a
`LanguageProperty` are `400`; `$metadata` lists the last two as
`NonInsertableProperties` and `NonUpdatableProperties`. Deep insert, deep
update, a bind on a collection navigation, `POST` or `DELETE` of one
`$ref` in a collection (no operation removes one object of a
Relationship) and `PUT …/$value` are `501`, and so are `If-Match`,
`If-None-Match` and a query option on a write. A property write that
names `id`, `scope`, a computed member, a `LanguageProperty` or a URL
member is `400` before any operation.

### 3.1 `$batch`

`POST $batch` takes a `multipart/mixed` (Protocol 11.7.7) or a JSON
(JSON Format 19) batch and answers in the same format. Each member is
served as a request of its own: parsed, refused and gated as it would be
alone, under the outer request's tenant, subject and credentials. Members
run in order. In a multipart batch the first failed member ends the batch
unless `Prefer: odata.continue-on-error` (or `continue-on-error`) is sent
(Protocol 11.7.7.4). A JSON batch runs every member unless
`continue-on-error=false` is sent (JSON Format 19.4). A member whose
`dependsOn` failed answers `424` (JSON Format 19.1). The response carries
`Preference-Applied` when the preference asked to go on.

A JSON batch is a 4.01 format: with `OData-MaxVersion: 4.0` it is `400`;
without the header it reads.
The multipart framing reads CRLF or a bare LF as a line end; a lone CR is
`400`, and a part's body keeps its bytes.

The whole batch is `400` before any member runs when a member URL leaves
this view's service root (another host, view or root, a `.` or `..`
segment, an encoded `/` or `\`), when a member sets a header other than
`Accept`, `Content-Type`, `Content-Length`, `If-Match`, `If-None-Match`,
`OData-Version`, `OData-MaxVersion` and a `Prefer` of `return` and
`odata.*` preferences, past 100 members (or `ANTARES_MAX_BATCH_ITEMS`
when lower), or when a member's JSON body nests deeper than 16. Change
sets, `atomicityGroup` and Content-ID references (`$1/...`) are `501`; a
batch inside a batch is `400`.

## 4. `$filter`

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

`ne` holds on an absent member, as in OData. A literal compared with a
member, or listed in `in`, is of the member's type or the filter is `400`
(URL 5.1.1.1, T-3725): `Name eq 1` and `Price eq 'abc'` are refused, a
number promotes among the numeric types (`Quantity lt 7.5`), an integer
past its type's range is refused (`Size eq 9223372036854775808`), and
`null` compares with any member. A number past 2^53 is `400` (NGSI-LD
compares `f64`). A filter longer than 4096 bytes as `q` is `400`
`TooComplexQuery`. Lambda operators, arithmetic, other functions, two
members compared and paths past one hop are `501`.

## 5. `$expand`

`$expand=owner`, `*`, `owner/$ref` and a nested `$select` are served one
level deep. The join names only the expanded Relationships, so a
navigation that is not expanded costs nothing. A target the view or the
engine hides, or one of another type, reads like an absent one: `null` for
a single navigation, left out of a collection. `/$ref` writes the
Relationship's object as an entity reference. A walk past the broker's
budget of 1000 lookups is `400`, never a partial answer. A nested
`$expand`, `$levels` and the other nested options are `501`.

## 6. Capabilities

`$metadata` annotates each EntitySet with what the surface serves:
`Insertable`, `Updatable` and `Deletable` as the view's `operations` offer
Create, Merge or Replace, and Delete Entity, with the members no write
names; `Countable`, `Filterable` and `Sortable` with the members they
refuse (`NonFilterableProperties`, `NonSortableProperties`), `Expandable`
one level, no upsert, deep insert, deep update or search, and
`QuerySegmentSupported`. The container declares `BatchSupported` and
`BatchSupport` (`multipart/mixed` and `application/json`, continue on
error, no Content-ID references; T-3723). The annotations are advisory:
every request is still gated, and a read the engine denies is `403`
whatever they say.

Every `GET` answers `HEAD` too: the same status and headers, with the
`Content-Length` of the `GET` and no body, through the same gate (T-3723).

## 7. Refusals and errors

The OData features no NGSI-LD operation stands behind (matrix 7.1 of
`docs/analysis/operation-matrix.yaml`: `$apply`, `$search`, `$compute`,
actions, functions, `$crossjoin`, `$all`, delta, media, and the rest) are
refused with the status the matrix names, before any gate or store access.
An upsert is the exception: it is the Merge or Replace Entity it names,
gated, and answers that operation's `404`. A custom query option is
`400`. A query a form encoder wrote reads as its plain form (T-3719):
`%24filter` is `$filter`, `%2F` and `%3D` are the query's `/` and `=`, and
`+` is a space where the grammar requires whitespace (`Price+gt+5`), never
inside a literal; a write method where it has no meaning (`/`, `$metadata`,
`$count`, a `POST` to one Entity, a `DELETE` of a set) is `405`, and the
write forms of later rounds are `501`.

An error is `{"error": {"code", "message"}}` with a fixed message: it
names no hidden type or Attribute, echoes no literal of the request and
carries no NGSI-LD `ProblemDetails`.

## Related

- [Endpoint views](views.md)
- [MCP endpoint of a view](mcp.md)
- [OData compliance](odata-compliance.md): the independent suite, its baseline and pass rate per level
- `docs/analysis/odata-linkml-mapping.md`, `docs/analysis/surfaces-threat-model.md` (O1 to O27)
- ADR-0024 (the OData crates)
