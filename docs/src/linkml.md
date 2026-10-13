# LinkML schema of a view

A `linkml` build (`cargo build -p antares-broker --features linkml`) serves
the schema of every loaded [view](views.md) that carries one:

```text
GET /x/views/{view}/linkml/...
```

Mount it with `ANTARES_API_SURFACES=admin,views,linkml`. It shares the
view root with the `views` surface (ADR-0027 section 1). The design is
`docs/analysis/odata-linkml-mapping.md` section 8 (T-3441); this page is
what the build serves today (T-3511).

## 1. Where the schema comes from

The schema stored for the view's Tenant at `/q/schemas/{tenant}`: LinkML
in its resolved JSON form ([admin API](admin-api.md), T-3628). Every view of
a Tenant serves the same source, each narrowed by its own rules. The surface
reads it with the `linkml-model` subset, so a metaslot outside the subset
(`imports`, for one) closes the LinkML surface and the reason goes to the
log (LK4). A Tenant without a schema answers `404` on every path, like a
view that is not loaded; a schema over `ANTARES_SCHEMA_MAX_BYTES` is
refused with `413` when it is written and never reaches the surface; one
stored under a bound above 1 MiB closes the surface and goes to the log (LK7).

## 2. What a caller sees

Every document is rendered from the schema narrowed to what the caller may
see through the view and the policy engine (`effective = view ∧ engine`,
ADR-0023 rule 5), never from the stored schema (LK1):

- a class is kept when its type (`class_uri`, else the default prefix and
  its name) is one a rule of the view, composed with the engine's answer,
  shows;
- a slot is kept on a class when a rule for that type shows the Attribute
  (`slot_uri`, else the default prefix and its name), less the engine's
  `pick`, `omit` and `omit_by_type`; an engine's grant over the whole
  Entity (a Policy naming the type without `propertyNames`) shows every
  slot of its types; a rule's condition is not read;
- `is_a` stays only when the parent is kept and shows nothing the child
  does not;
- a range to a hidden class becomes `uriorcurie`, with the slot's kind
  stated as `ngsi_ld_kind: Relationship` (DM-69); `inverse` stays only
  when both ends are kept;
- only the enums a kept slot uses and the prefixes a kept element names
  are listed (LK6);
- the narrowed schema's `id` is `<source id>/views/<view>`, so it never
  claims to be the full model.

## 3. Paths

Each path is one discovery operation of the view, asked through
`antares-ops` with its own clause; a view whose `operations` leave it out
answers `405`, and an engine that denies it `403`. A denied
`classes/{name}` or `slots/{name}` answers the `404` of an absent name, so a
refusal never tells that the name is there (LK1).

| Path | Content | Operation |
|---|---|---|
| `schema/index.json` | the model with its `types` and each document's media type, size and SHA-256; `redacted: true` when anything was left out | Retrieve Entity Type Details (5.7.6) |
| `schema/v{major}/model.linkml.yaml` or `.../linkml` | LinkML YAML | 5.7.6 |
| `schema/v{major}/model.schema.json` or `.../json-schema` | JSON Schema | 5.7.6 |
| `schema/v{major}/context.jsonld` or `.../context` | JSON-LD `@context` of the kept terms | 5.7.6 |
| `schema/v{major}/model.shacl.ttl` or `.../shacl` | SHACL | 5.7.6 |
| `schema/v{major}/model.owl.ttl`, `model.rdf.ttl`, `model.md` (`owl`, `rdf`, `docs`) | OWL, RDFS, Markdown | 5.7.6 |
| `schema/v{major}/model.qb.ttl` or `.../qb` | the RDF Data Cube structure, only when a kept class is a Data Structure Definition (below) | 5.7.6 |
| `schema/v{major}/model` | the formalism `Accept` names (Turtle is SHACL, `profile="owl"` OWL, YAML LinkML, Markdown, `ld+json` the context, else JSON Schema) | 5.7.6 |
| `classes/{class}` | one class with its slots (JSON) | Retrieve Entity Type Information (5.7.7) |
| `slots/{slot}` | one slot with its range, kind and the classes using it (JSON) | Retrieve Attribute Information (5.7.10) |

`{major}` is the leading number of the schema's `version` (`1` when it has
none). A hidden class or slot, an unknown major, the cube of a model whose
kept classes hold no Data Structure Definition, and any other path answer
`404` with the same body as one that does not exist (LK8).

A class annotated `qb_dsd: "true"` is an RDF Data Cube Data Structure
Definition (DM-60), and its slots annotated `qb_component: dimension` or
`measure` are its components; any other value of either makes the schema
one the subset cannot read, which closes the surface (LK4). `model.qb.ttl` has one `qb:DataStructureDefinition` per kept
class that is one, its components the kept slots with a role, the class a
subclass of `qb:Observation`; the OWL types the same class and slots. A
dimension the view or the engine hides is no component, and a hidden
Data Structure Definition leaves no cube: the index lists `model.qb.ttl`
only when it exists.

## 4. Caching

- Every answer carries a strong `ETag` (the SHA-256 of its bytes) and
  `Cache-Control: private, no-cache`; a matching `If-None-Match` is `304`.
- `HEAD` answers as `GET` without the body.
- `POST`, `PUT`, `PATCH` and `DELETE` are `405` with `Allow: GET, HEAD`
  (LK5).
- A pod renders each schema once per view, stored schema and visible set,
  and keeps the 256 latest; the engine is asked on every request, so a
  policy change shows on the next one (LK2).

`cargo bench -p antares-linkml --bench schema` measures a 100-class schema:
a document from the cache, and the first request after the view changes,
which reads, narrows and renders it.
