# joinedcontext-policy

The policy engine of the joinedcontext platform (engine `joinedcontext`,
broker feature `jc-policy`, `ANTARES_POLICY=joinedcontext`). An addon by
ADR-0020: it is never in the release image, never in a conformance cell,
and `dev/check-no-addon.sh` keeps it out of a default build. It decides on
the `Operation` and the `Subject` alone: no network, no store, no token.
Plan: `docs/analysis/platform-switch-plan.md` sections 3.1 to 3.3.

## 1. The subject

The broker copies the headers `ANTARES_POLICY_SUBJECT_HEADERS` names into
the `Subject`; the platform's context-gateway sets them from the verified
token and strips them from every client request.

| Header | Meaning |
|---|---|
| `x-jc-subject` | the user (the token's `preferred_username`) |
| `x-jc-client` | the service account (the client id) |
| `x-jc-groups` | groups, comma separated |
| `x-jc-roles` | roles, comma separated; the Endpoint roles (`endpoint:{project}/{endpoint}[/{role}]`) the gateway resolved for this request among them |
| `x-jc-restricted` | `true` when the caller opted into the restricted signal (O-3 (b)) |
| `x-jc-did` | the data-space participant a transfer token names (platform DS-02, map K38) |

Each value, and each member of a list, is percent-encoded UTF-8 (RFC 3986:
the unreserved characters `A-Z a-z 0-9 - . _ ~` stay as they are, every
other byte is `%XX`), so a name with a diacritic or a comma travels whole
(`doprava/správa` is `doprava%2Fspr%C3%A1va`). The engine splits a list on
`,` first and decodes each member after, so an encoded comma never splits a
name. A member that does not decode (a broken escape, bytes that are not
UTF-8) is dropped alone and the rest kept; a `x-jc-subject` or
`x-jc-client` that does not decode names nobody and still makes the caller
not anonymous. Unencoded ASCII names read as before.

A request with at least one of these headers is a platform caller. One with
`x-jc-did` is a data-space participant: it holds no role and no group,
whatever `x-jc-roles` and `x-jc-groups` say beside it, so only the Policies
compiled from its agreement (assignee `did`) grant it anything. One with no
`x-jc-subject`, no `x-jc-client` and no `x-jc-did` is anonymous and holds the
role `public`, whatever `x-jc-roles` says beside it.

A request with none of them comes from inside the cluster (the Portal's
registration sync, plan 5.2 M22; nothing else reaches the broker, the
NetworkPolicy admits the gateway and the Portal only). Decided (worker-1,
owner may override): such a request is allowed on the Context Source
clauses `5.9.*`, `5.10.*` and `5.11.*` and denied on every other one, so a gateway
that forgot the subject fails closed. Options: deny it everywhere (breaks
M22), allow it everywhere (a forgotten header opens the tenant).

## 2. The rule document

One document per Tenant, written by the Portal with
`PUT /q/policy/{tenant}` (`docs/src/admin-api.md`), compiled from the
space's Policy manifests (platform `PolicySpec`). JSON, unknown members
refused:

```json
{
  "policies": [
    {
      "id": "city/read-cars",
      "effect": "permit",
      "assigner": "did:web:city.example.org",
      "assignee": {"kind": "role", "id": "public"},
      "operations": ["5.7.1", "5.7.2"],
      "validity": {"from": "2026-01-01T00:00:00Z", "to": "2027-01-01T00:00:00Z"},
      "information": [
        {
          "entities": [{"type": "https://example.org/Car", "idPattern": "^urn:ngsi-ld:Car:a.*$"}],
          "propertyNames": ["https://example.org/speed"],
          "relationshipNames": []
        }
      ],
      "q": "speed<100",
      "scopeQ": "/geo/SK/BB",
      "geoQ": "georel=within;geometry=Polygon;coordinates=[[[0,0],[1,0],[1,1],[0,1],[0,0]]]",
      "temporalQ": "timerel=after;timeAt=P-1D"
    }
  ]
}
```

| Member | Rule |
|---|---|
| `id` | required, unique in the document; named by the access surface and in a load error, never in a refusal |
| `effect` | `permit` (default) or `prohibit` |
| `assigner` | optional: the party that granted it (a DID or URL), shown by the access surface (section 4); decides nothing |
| `assignee.kind` | `user` (`x-jc-subject`), `group`, `role`, `serviceAccount` (`x-jc-client`); `did` (`x-jc-did`) |
| `operations` | CIM 009 clauses (`5.6.1`, `5.7.2`, ...); the compiler expands the platform's operation names and groups |
| `validity` | optional half-open `[from, to)`, RFC 3339 |
| `information` | the grant's selectors; each `entities` member has a `type` (an IRI or a core term), at most one of `id` and `idPattern`; `propertyNames` and `relationshipNames` are the Attributes it shows, none = the whole Entity |
| `q`, `scopeQ` | NGSI-LD query and scope query, on the Entity |
| `geoQ` | `georel=…;geometry=…;coordinates=…[;geoproperty=…]` |
| `temporalQ` | `timerel=before|after|between;timeAt=…[;endTimeAt=…]`, an instant or an ISO 8601 duration before now (`P-1D`) |
| `views` | optional, beside `policies`: `{"{viewId}": ["policy id", …]}`. A request through a listed view (the subject's `@view`, ADR-0020 amendment) is decided by those policies alone; a view not listed, and the plain NGSI-LD root, by every policy. Each id must name a policy of the document, at most once per view; a view may list none (it then grants nothing) |

A document that does not parse is refused and the previous one keeps
deciding; a Tenant with no document denies every platform caller.

## 3. Decisions

1. The policies that apply: those of the request's view when the document
   lists it (`views`), every policy otherwise; of those, the ones whose
   assignee is the caller, whose clause is listed and whose validity holds
   now.
2. A prohibition that applies and names no type, or a type the operation
   names, or applies to an operation naming no type: `Deny`.
3. No permission applies: `Deny` (default deny).
4. A read (`policy::FILTERABLE`): `Filter` with one `Rule` per selector of
   every permission: its type, id or id pattern, the information's
   Attributes, the policy's `q`, `scopeQ`, `geoQ` and `temporalQ` window.
   An Entity no rule holds for is not served; one rule holds = it shows
   that rule's Attributes (ADR-0023 rule 11). A permission without
   `information` grants every type: its rules take the operation's types,
   and with none named and a condition on it the read is refused (the
   engine cannot write a rule for every type). One without information and
   without condition allows the read whole.
   `restricted` is set only with `x-jc-restricted: true` and a filter that
   narrows.
5. A write is performed whole or refused whole. It must fall inside one
   permission: the Entity's types and id selected by one of its selectors,
   every Attribute it writes shown by that selector's information, its
   `scope` inside the policy's scope tree (path prefix, `/a/b` covers
   `/a/b/c` and never `/a/bc`), its `location` inside the policy's area, no
   access-control member (`owner`, `acl`, `allowedRoles`, `visibility`,
   `permissions`, `policy`). A create (5.6.1, 5.6.7) also meets the policy's
   `q`. A write by id (`policy::WRITE_BY_ID`) under a permission with a `q`,
   `scopeQ` or `geoQ` answers `AllowIf`: the store judges the stored Entity.
   A Batch Upsert member (5.6.8) is such a write: the store holds an
   update to the condition and refuses a create under one (T-3668).
6. Every other clause (subscriptions, registrations, snapshots, ...):
   `Allow` when a permission applies, as decided above.
7. A refusal says `no policy grants this operation` or `a policy prohibits
   this operation` and names no policy (platform GW6).
8. A batch is decided member by member (`decide_each`); a notification is
   decided again for the Subscription's stored subject as a query (5.7.2)
   naming the types of the Entities it carries: `Deny` drops it, a `Filter`
   narrows it (T-3784).
9. The tool probe (`offers_each`, MCP D-3) asks whether some request of a
   clause could be allowed, with an Operation naming nothing. A prohibition
   without type refuses every clause it lists. Otherwise a read is offered
   when a permission applies (a read of a type no prohibition names is
   narrowed, never refused, rules 2 and 4); a write when a permission
   without information applies, or one naming a type no prohibition covers
   (a conditional prohibition counts as covering its whole type); a
   whole-Tenant clause is decided as a request. A call is still decided by
   rules 1 to 7 (T-3784).

## 4. The access surface

`ANTARES_API_SURFACES=...,access` mounts `/x/views/{view}/access`, built
with `jc-policy` and `views` (the platform image's `full,jc-policy`; plan
section 5.6). It answers "what may I do through this
view" from the same compiled table the engine decides with, narrowed to
the view's policies when `views` lists it, so the
description and the enforcement cannot drift (platform EP-60). The view
names the Tenant; an unknown view is 404, the same as on every view
surface.

| Route | Answer |
|---|---|
| `GET access` | the caller's grants; the representation the `Accept` header names first, `application/json` otherwise (never 406) |
| `POST access/check` | AuthZEN: `{"action": {"name": …}, "resource": {"type": …}}` → `{"decision": true, "context": {"reason": "policy_grant_matched", "assigner": …}}` or `{"decision": false}` |

| `Accept` | Representation |
|---|---|
| `application/json` (default) | AuthZEN resource search: `subject`, `resource` (`{"type": "view", "id": …}`), `permissions`, `prohibitions`, one entry per selector with `resource`, `actions`, `attributes` (`"*"` for the whole Entity), `constraints` (`q`, `scopeQ`, `geoQ`, `temporalQ` as written) |
| `application/odrl+json` | ODRL 2.2 `Set` in the `ngsi-ld:` profile; its `uid` carries the SHA-256 of its rules |
| `text/turtle` | the same ODRL policy as RDF, written from the JSON-LD |
| `application/vnd.joinedcontext.grant-ast+json` | UCAST condition tree per Entity type: `operations`, `project`, `q` verbatim, `where` from `scopeQ`, `geoQ`, `temporalQ`; prohibited operations taken off |

Actions are the CIM 009 Table 4.20-1 names (`queryEntity`, `createEntity`,
...): each clause a policy lists, restricted to the names whose
operation-matrix rows the view allows, under the clause the engine is
asked for those rows: `retrieveEntityTypeDetails` and
`retrieveAttrTypeDetails` are asked as 5.7.5 and 5.7.8, on the routes they
share with the lists. A policy without `information` shows the type `*`.

What the caller holds and nothing else: only policies whose assignee is the
caller and that are in force now; a type only somebody else may touch is
absent, never listed as denied; a refusal of `access/check` names nothing
(GW6). A request with no `x-jc-*` header is the cluster's own and holds no
policy, so its document is empty.
