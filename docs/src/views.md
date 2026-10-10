# Endpoint views

A `views` build (`cargo build -p antares-broker --features views`) serves
each loaded view as an NGSI-LD API root of its own:

```text
/x/views/{view}/ngsi-ld/v1/...
```

Mount it with `ANTARES_API_SURFACES=admin,views` and load views through
[`/q/views`](admin-api.md#get-qviews). Until the first view loads, every
path under `/x/views` answers `503` (ADR-0023 rule M4); a vanilla build has
no `/x/views` at all.

## 1. What a view serves

Every request below the view root runs as the broker's own operation
(`antares-ops`), narrowed by the view (ADR-0023) and in the view's one
Tenant:

| Clause | Operation |
|---|---|
| 10.4.2, 10.4.3, batch query | Retrieve and Query Entities |
| 10.4.4 | Retrieve Attribute Value |
| 10.6.3 to 10.6.8 | Entity types and Attributes |
| 11.3.2, 11.3.3 | Temporal Retrieve and Query |
| 13.3 | List @contexts: the core context only |
| 10.2.2 to 10.2.11, but 10.2.12 Purge | Create, Update, Append, Partial Update, Set Value, Delete Attribute, Delete, Merge, Replace Entity, Replace Attribute |
| 10.3.2, 10.3.4 to 10.3.6 | Batch create, update, merge and delete, member by member |
| 10.5.2 to 10.5.6 | Create, Update, Retrieve, Query and Delete Subscription |

A view serves an operation only when its `operations` list names it.
Batch Upsert, Purge and the temporal writes are not served through a
view.

## 2. What a client sees

- An Entity is in the view when a rule names its type and the view's `q`,
  `scopeQ` and areas hold for its current state; the history of an Entity
  out of the view now is not served (rule 8).
- Hidden Attributes are absent from every answer, and a `q`, `orderBy` or
  `pick` on one matches as if it were absent: the answer is the same
  whatever the hidden values are.
- `NGSILD-Tenant` may be omitted or name the view's Tenant; any other value
  is `400`. The answer echoes the header only when the client sent it.
- `Link`, `Location` and `Content-Location` name the view root, so a next
  page stays inside the view.

## 3. Writes

A write through a view runs on the local store only, never forwarded to a
Context Source, and the broker checks it on the stored Entity inside the
same store operation (ADR-0023 rule 7):

- The Entity must be in the view before the write. One out of the view
  answers `404`, byte for byte as an absent one, before any type selector
  or body check.
- Every Attribute the write names must be one a rule holding for the
  Entity shows, or the whole write is `403`. The answer is the same
  whether the Entity holds that Attribute or not. A Replace Entity also
  counts every Attribute the stored Entity holds, so a replace cannot
  delete what the view hides.
- The Entity must still be in the view after the write, or the write is
  `403` ("the write would move the Entity out of this view").
- A create must produce an Entity the view selects. An id taken out of
  the view is `409`, the same body an id taken in the view gets.
- A Relationship must point at an Entity the view shows. A hidden target
  and an absent one both answer `403`.
- A batch applies these checks to each member and answers the
  `BatchOperationResult`, `207` when mixed.

A refused write leaves the store as it was.

## 3a. Subscriptions

A Subscription made through a view is stored as the client sent it, with
the view beside it, and the broker applies the view each time it matches
and notifies (ADR-0023 rule 9). Narrowing or removing the view takes
effect on the Subscriptions already made:

- Its conditions read only what the view shows of the Entity now, and an
  Entity the view does not select does not match.
- A change only to hidden Attributes sends nothing and leaves `timesSent`
  and `lastNotification` as they were.
- A notification carries the shown Attributes only; a hidden Attribute's
  `previousValue` goes with it.
- A removed view delivers nothing.
- `watchedAttributes` keeps only the Attributes the view shows, at create
  and at update; none left is `400`.
- Only the view a Subscription was made through reads, lists, updates or
  deletes it; another view answers `404`. The root sees every
  Subscription of its Tenant.

## 4. Refusals

| Answer | When |
|---|---|
| `404` | unknown, broken or removed view id (all alike); a resource the view serves nothing on |
| `405` + `Allow` | the view serves other methods on this resource |
| `422` | the view serves this method here, but not this operation of it (`?details=true`, for example) |
| `400` | the request names an entity map (`entityMap`, `NGSILD-EntityMap`): maps are not served through a view |

The view id is the raw path segment: `CARS`, `ca%72s` or `x/../cars` never
reach the view `cars`. No refusal names a hidden Attribute or an inner
failure; a broker error inside the view is a `500` with a request id only.

## 5. The Tenant's schema

A surface that needs the data model of a view's Tenant (OData's Edm types
and navigation properties, the LinkML surface) reads the Tenant's `LinkML`
schema, written to [`/q/schemas/{tenant}`](admin-api.md#schemas). Every view
of that Tenant sees the same schema and no view sees another Tenant's. The
surface gets the bytes and a revision through `ViewOps::schema`, parses
them under its own limits, and parses again only when the revision changes.
