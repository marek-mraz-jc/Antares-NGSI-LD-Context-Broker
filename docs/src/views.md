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

A view serves an operation only when its `operations` list names it.
Batch Upsert, Purge and the temporal writes are not served through a
view; Subscriptions through a view come with T-3499.

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
