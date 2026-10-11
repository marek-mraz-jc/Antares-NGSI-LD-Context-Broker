# MCP endpoint of a view

An `mcp` build (`cargo build -p antares-broker --features mcp`) serves
every loaded [view](views.md) to Model Context Protocol clients:

```text
POST /x/views/{view}/mcp
```

Mount it with `ANTARES_API_SURFACES=admin,views,mcp`. It shares the view
root with the `views` surface (ADR-0027 section 1) and answers `404` for a
view that is not loaded. The design is `docs/analysis/mcp-surface.md`
(T-3442); this page is what the build serves today (T-3513).

## 1. Protocol

- Streamable HTTP, JSON answers, no sessions and no server-sent stream.
- Revision `2026-07-28` first: every request carries its own protocol
  metadata in `_meta`, and `server/discover` answers the versions served.
- The legacy `initialize` of `2025-11-25` and `2025-06-18` works without a
  session and lists the same tools as the modern era, no more (MC13).
- A JSON-RPC batch array is refused whole with `-32600` (MC6).
- `Mcp-Method` and `Mcp-Name` headers that disagree with the body are
  `400` before dispatch (MC14).
- The `Host` must be one of `ANTARES_MCP_ALLOWED_HOSTS` (loopback names
  when unset), and a request with an `Origin` outside
  `ANTARES_MCP_ALLOWED_ORIGINS` is `403` (MC8).
- The request body is at most 1 MiB.

## 2. Tools

Each tool is one read operation of the view, run through `antares-ops`, so
its policy gate fires and the view narrows the answer as on the view root.
`tools/list` names a tool only when the view's `operations` allow it and
the policy engine's probe does (D-3); a tool hidden either way answers like
one that does not exist (`-32602 unknown tool`, MC1).

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

Every tool is annotated read-only. Write tools come with T-3514.

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

## 3. Size and paging

One result's structured content is at most `ANTARES_MCP_MAX_RESULT_BYTES`
(64 KiB by default). A paged tool takes `limit` (default 20, at most 1000),
`count` and `cursor`:

- a page holds the rows that fit the budget, whole and in order; when rows
  remain it carries `nextCursor`, and `total` when `count` was asked;
- a cursor is opaque, at most 2048 characters, and works only for the tool
  and the arguments it was made for (MC20);
- one item past the budget is a tool error naming `pick` or `attrs` (or
  `lastN` for history); nothing is ever cut (MC5).

## 4. Resources

| URI | What |
|---|---|
| `ngsi-ld://{view}/types/{type}` | the type's information, one resource per type the view shows |
| `ngsi-ld://{view}/entities/{id}` | a template: one Entity, its id percent-encoded or not |

A URI of another view, a malformed one and one whose Entity is out of view
or absent all answer `-32602 unknown resource` (MC10). An Entity past the
result budget is `-32000`; read it with `get_entity` and `pick`.

## 5. Caching

`server/discover` is `cacheScope: public` (one hour); every other result
is `private`: the tool list for a minute, the resource templates for five,
resource lists and reads not at all (MC9).

## 6. Logging

Each tool call logs one line with the view, the tool, the operation id and
the status. The argument values are never logged (MC19).

## Related

- [Endpoint views](views.md)
- [Configuration](configuration.md): `ANTARES_MCP_*`
- `docs/analysis/mcp-surface.md`, `docs/analysis/surfaces-threat-model.md` section 5
