# MCP endpoint of a view

An `mcp` build (`cargo build -p antares-broker --features mcp`) serves
every loaded [view](views.md) to Model Context Protocol clients:

```text
POST /x/views/{view}/mcp
```

Mount it with `ANTARES_API_SURFACES=admin,views,mcp`. It shares the view
root with the `views` surface (ADR-0027 section 1) and answers `404` for a
view that is not loaded. The design is `docs/analysis/mcp-surface.md`
(T-3442); this page is what the build serves today (T-3513 to T-3515).

## 1. Protocol

- Streamable HTTP, JSON answers, no sessions; only `subscriptions/listen`
  answers a server-sent stream (section 6).
- Revision `2026-07-28` first: every request carries its own protocol
  metadata in `_meta`, and `server/discover` answers the versions served.
- The legacy `initialize` of `2025-11-25` and `2025-06-18` works without a
  session and lists the same tools as the modern era, no more (MC13).
- A JSON-RPC batch array is refused whole with `-32600` (MC6); a body that
  is not JSON answers `-32700` and a message other than JSON-RPC 2.0
  `-32600`, each as an error frame with HTTP 400.
- `Mcp-Method` and `Mcp-Name` headers that disagree with the body are
  `400` before dispatch (MC14).
- The `Host` must be one of `ANTARES_MCP_ALLOWED_HOSTS` (loopback names
  when unset), and a request with an `Origin` outside
  `ANTARES_MCP_ALLOWED_ORIGINS` is `403` (MC8).
- The request body is at most 1 MiB.

## 2. Tools

Each tool is one operation of the view, run through `antares-ops`, so
its policy gate fires and the view narrows the answer as on the view root.
`tools/list` names a tool only when the view's `operations` allow it and
the policy engine's probe does (D-3); a tool hidden either way answers like
one that does not exist (`-32602 unknown tool`, MC1).

A view of another model (one with a `mapping`, [views](views.md)) is
served too (T-3817): its tools are the reads a mapping runs, every
argument and answer is in the target model, and the input schemas name the
target class and the slots the view can produce from what it shows, never
a source name. Each call is inverted and rebuilt as on the view root.

| Tool | Operation | Paged |
|---|---|---|
| `batch_query` | Query Entities, POST form (10.4.3) | yes |
| `batch_query_temporal` | Temporal Query, POST form (11.3.3) | yes |
| `get_attribute` | Retrieve Available Attribute Information (10.6.8) | no |
| `get_attribute_value` | Retrieve Attribute Value (10.4.4) | no |
| `get_entity` | Retrieve Entity (10.4.2) | no |
| `get_subscription` | Retrieve Subscription (10.5.4) | no |
| `get_type` | Retrieve Available Entity Type Information (10.6.5) | no |
| `list_attribute_details` | Retrieve Available Attribute Details (10.6.7) | no |
| `list_attributes` | Retrieve Available Attributes (10.6.6) | no |
| `list_subscriptions` | Query Subscriptions (10.5.5) | yes |
| `list_type_details` | Retrieve Available Entity Type Details (10.6.4) | no |
| `list_types` | Retrieve Available Entity Types (10.6.3) | no |
| `query_entities` | Query Entities (10.4.3) | yes |
| `query_temporal` | Temporal Query (11.3.3) | yes |
| `retrieve_temporal` | Temporal Retrieve (11.3.2) | no |

The write tools (section 3) are listed the same way:

| Tool | Operation | Arguments |
|---|---|---|
| `append_attributes` | Append Attributes (10.2.4) | `id`, `attributes`, `noOverwrite` |
| `batch_create` | Batch Entity creation (10.3.2) | `entities` (at most 100) |
| `batch_delete` | Batch Entity delete (10.3.6) | `ids` (at most 100) |
| `batch_merge` | Batch Entity merge (10.3.5) | `entities` |
| `batch_update` | Batch Entity update (10.3.4) | `entities`, `noOverwrite` |
| `batch_upsert` | Batch Entity upsert (10.3.3) | `entities`, `options` |
| `create_entity` | Create Entity (10.2.2) | `entity` |
| `create_subscription` | Create Subscription (10.5.2) | `subscription` |
| `delete_attribute` | Delete Attribute (10.2.7) | `id`, `attrId`, `datasetId`, `deleteAll` |
| `delete_entity` | Delete Entity (10.2.8) | `id` |
| `delete_subscription` | Delete Subscription (10.5.6) | `id` |
| `merge_entity` | Merge Entity (10.2.9) | `id`, `fragment` |
| `replace_attribute` | Replace Attribute (10.2.11) | `id`, `attrId`, `attribute` |
| `replace_entity` | Replace Entity (10.2.10) | `id`, `entity` |
| `update_attribute` | Partial Attribute update (10.2.5) | `id`, `attrId`, `attribute` |
| `update_attributes` | Update Attributes (10.2.3) | `id`, `attributes` |
| `update_subscription` | Update Subscription (10.5.3) | `id`, `fragment` |

A read tool is annotated `readOnlyHint: true`, a write tool `false`; a write
that can remove or overwrite is `destructiveHint: true`, and the three that
only add (`create_entity`, `create_subscription`, `batch_create`) say `false`.

- **Input schemas** name only the types and Attributes the view shows
  (D-4), as an `enum` up to 256 names. An unknown argument, a hidden name
  and an absent one are refused alike; a refusal names the argument and
  the rule, never the value. No argument names a tenant, a space or a
  subject: the tenant is the view's (MC15).
- **History.** The temporal tools take `timerel`, `timeAt`, `endTimeAt`,
  `lastN` and `timeproperty`; history without `observedAt` needs
  `timeproperty: "modifiedAt"` (or `createdAt`).
- **Joins.** `join` and `joinLevel` are arguments of `get_entity` and
  `query_entities`: the view checks every join target (T-3470), so a join
  never reaches an Entity out of view (MC16).
- **Results** are `structuredContent` under one key (`entity`,
  `entities`, ...) plus the same JSON as a text block. Descriptions and
  instructions are fixed strings; no stored value ever appears in one
  (MC11).
- **Errors.** A failed operation is a tool result with `isError: true`,
  text `"{status} {title}"` and structured `{"error": {type, title,
  status}}`; the problem's `detail` stays inside (MC18). An operation the
  engine denies is such an error, never an empty list (MC3).

## 3. Confirmed writes

A write changes data only after the person confirmed it, not the model
(MCP 3.4, D-11, MC4). The round trip is the 2026-07-28 multi round-trip
request:

1. The first call checks the arguments and answers an
   `InputRequiredResult`: one `elicitation/create` form request under the
   key `confirm`, asking a required boolean `confirm`, and a
   `requestState`. The question names the tool, its operation, the Entity
   ids, the Attribute names and a Subscription's endpoint; never an
   Attribute value. Nothing is written.
2. The client asks the person and calls again with the same arguments,
   `inputResponses: {"confirm": {"action": "accept", "content":
   {"confirm": true}}}` and the `requestState` it was given.
3. The retry runs the operation through its policy gate as any call does:
   the confirmation is in addition to the gate, never instead of it.

The `requestState` is sealed with HMAC-SHA256 over the view, the caller's
policy subject headers, the tool and its arguments, and expires after ten
minutes. A state that was changed, made for other arguments, another tool,
another view or another caller, or that was already used once is refused
with one tool error, and the call writes nothing. A "no", a decline or a
cancel is a tool error too.

- **The key.** `ANTARES_MCP_REQUEST_STATE_KEY_FILE` names a file of at
  least 32 bytes, mounted from a Kubernetes Secret, so every replica
  accepts the others' states. Unset, each process draws its own key at
  start: a retry that reaches another replica is refused, and the client
  asks again. A named file that cannot be read or is too short refuses
  every write and logs one error; the key itself is never logged.
- **Spent states** are remembered per process until they expire, at most
  100 000 at a time; past that a confirmation is refused until the oldest
  expire.
- **Clients.** A client that does not declare `elicitation` (form) in its
  `io.modelcontextprotocol/clientCapabilities`, and every legacy
  `initialize` client, gets the tool error `this tool needs a client that
  can ask the person`.
- **Bodies.** An Entity, a fragment and a batch member name only the
  types and Attributes the view shows; `@context` is refused, the view's
  is used. Under a view the broker checks every write on the stored
  Entity (ADR-0023 rule 7), a batch member by member.
- **Subscriptions.** `create_subscription` and `update_subscription`
  accept only the members `type`, `subscriptionName`, `description`,
  `entities`, `watchedAttributes`, `q`, `notification` (`attributes`,
  `format`, `endpoint.uri`, `endpoint.accept`), `expiresAt`, `throttling`
  and `isActive`. `endpoint.uri` must be one of
  `ANTARES_MCP_SUBSCRIPTION_ENDPOINTS`, or below one of its entries that
  ends in `/`. Unset, every endpoint is refused, before the person is
  asked (MC12).
- **Results.** A write with an empty answer returns `{}`, or `{"created":
  id}` when the broker named what it made.

## 4. Size and paging

One result's structured content is at most `ANTARES_MCP_MAX_RESULT_BYTES`
(64 KiB by default). A paged tool takes `limit` (default 20, at most 1000),
`count` and `cursor`:

- a page holds the rows that fit the budget, whole and in order; when rows
  remain it carries `nextCursor`, and `total` when `count` was asked;
- a cursor is opaque, at most 2048 characters, and works only for the tool
  and the arguments it was made for (MC20);
- one item past the budget is a tool error naming `pick` or `attrs` (or
  `lastN` for history); nothing is ever cut (MC5).

## 5. Resources

| URI | What |
|---|---|
| `ngsi-ld://{view}/types/{type}` | the type's information, one resource per type the view shows |
| `ngsi-ld://{view}/entities/{id}` | a template: one Entity, its id percent-encoded or not |

A URI of another view, a malformed one and one whose Entity is out of view
or absent all answer `-32602 unknown resource` (MC10). An Entity past the
result budget is `-32000`; read it with `get_entity` and `pick`.

## 6. Resource subscriptions

A 2026-07-28 client watches Entities with `subscriptions/listen` and
`notifications.resourceSubscriptions` (D-7). The answer is a server-sent
stream: the acknowledgment, then `notifications/resources/updated` with
the URI alone each time a watched Entity changes. The client reads it
again with `resources/read`, through the gate. No other notification type
is acknowledged, and a legacy `initialize` client cannot listen (D-8).

- **What is acknowledged.** `ngsi-ld://{view}/entities/{id}` URIs of this
  view, at most 64 distinct ones, and only when the view's `operations`
  allow `retrieve_entity`, `create_subscription` and
  `delete_subscription`; otherwise none. A `types/` URI or another view's
  is left out. Whether the Entity exists or is visible plays no part.
- **What is watched.** Each acknowledged Entity the caller can read is
  backed by one NGSI-LD Subscription the surface makes through the view as
  the caller (ADR-0023 rule 9): `watchedAttributes` and
  `notification.attributes` are the view's Attributes, `throttling` 1 s,
  `expiresAt` one hour on. An Entity that is absent or out of view is
  never watched and sends nothing. A change to an Attribute the view hides
  is no change in a watched one, so it sends no event (MC17).
- **Delivery.** The Subscription's endpoint is `x-antares-mcp:{stream}`,
  served by an in-process sink that opens no socket. Nobody can name that
  endpoint over HTTP or a tool: it is accepted only while the stream makes
  its own Subscription (MC7). A burst of changes to one Entity is one event
  until the stream has sent it.
- **Bounds.** A stream lasts at most one hour. One process holds at most
  4 streams per subject and Tenant (callers that send no subject header
  share one subject), 64 per Tenant and 256 in all; past a bound the
  stream is acknowledged and then ended with error `-32000` naming the
  bound.
- **Closing.** When the stream ends (the client goes away, the hour is
  over, the server stops), the surface deletes its Subscriptions; a
  process that dies leaves them to their `expiresAt`. A stream is held by
  the process that opened it: with several pods, a change notified on
  another pod is lost (mcp-surface 5.4).

## 7. Caching

`server/discover` is `cacheScope: public` (one hour); every other result
is `private`: the tool list for a minute, the resource templates for five,
resource lists and reads not at all (MC9).

## 8. Logging

Each tool call logs one line with the view, the tool, the operation id and
the status; a write that was not confirmed logs the view and the tool. The
argument values and the `requestState` are never logged (MC19).

## Related

- [Endpoint views](views.md)
- [Configuration](configuration.md): `ANTARES_MCP_*`
- `docs/analysis/mcp-surface.md`, `docs/analysis/surfaces-threat-model.md` section 5
