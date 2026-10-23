# Storage drivers

The broker holds storage behind two object-safe traits in the
dependency-free `antares-store` crate (`crates/antares-store/src/lib.rs`,
ADR-0013): the core names the traits and no backend.

| Trait | Surface | Chosen by |
|---|---|---|
| `CurrentStateDriver` | create / get / delete / list / upsert, the batch operations, `query_entities`, `entity_type_names`, `entity_types_beside` and `entity_attr_pairs` (the `/types` and `/attributes` lists, T-4610, T-4683, T-4934), `matching_registrations`, `sweep_expired`, tenant inventory and purge, `@context` documents, ping / close / version, commit-queue depth, change hook and outbox wiring. | `ANTARES_STORE` |
| `TemporalDriver` | `temporal_append`, `query_temporal`, `get_temporal`, the temporal delete paths (CIM 009 5.6.13 to 5.6.16), the temporal-entity documents, and the event intake (`event` / `event_list`) that the post-response drain feeds. `supported()` tells the API whether history exists at all, and `close()` / `version_info()` mirror the current-state seam — a temporal driver on a backend of its own has its own pool to drain and its own version to report. | `ANTARES_TEMPORAL` |

`AppState` carries one `Arc<dyn CurrentStateDriver>` and one
`Arc<dyn TemporalDriver>`. Both traits keep generic `mutate<T, E>` ergonomics
through the `*Ext` extension traits over a boxed `mutate_boxed`. Dynamic
dispatch runs a handful of times per request against a database round trip
or a JSON-LD expansion, so it costs nothing measurable.

## The store ladder

`memory → file → postgres → timescale` (ADR-0004): one binary, one
configuration value. The `mem/` and `pg/` folders under
`crates/antares-sql/src/store/` are the two implementations; `any.rs` is the
`AnyStore` dispatcher that implements both traits. The folder README
(`crates/antares-sql/src/store/README.md`) lists the steps for adding a
backend.

| Mode | Current state lives in | History lives in | Survives |
|---|---|---|---|
| `memory` | process maps | process maps | nothing; the unit-test default and the read-only-rootfs mode |
| `file` | the same maps, with a redb write-through shadow in `ANTARES_DATA_DIR/antares.redb` | the same file | process restart, `kill -9` after the 2xx (commit before ack) |
| `postgres` | one shared-schema database, `tenant_id` on every row under Row-Level Security | `attr_instances`, range-partitioned by `observed_at` | restart, replica failover, ordinary Postgres backup |
| `timescale` | the same schema | `attr_instances` as a hypertable (7-day chunks, native compression) | as `postgres` |
| browser (`wasm`) | the `memory` maps, or `AntaresBroker.persistentWithHandle(...)` over an OPFS sync-access handle | the same | the origin's private file system; the redb format is the native `file` one ([Browser & WebAssembly](wasm.md)) |

`file` is not a second store: the redb shadow holds one table per resource
family (`entities`, `subscriptions`, `csource_registrations`,
`csource_subscriptions`, `temporal_entities`, `jsonld_contexts`, `snapshots`,
`entity_map_docs`, `dist_subs`, `dead_letters`, plus `meta` for the format
version), keyed `tenant\0id`, value the expanded JSON. Every commit is
`Durability::Immediate` inside the store's write-critical section, so the
fsync completes before the HTTP answer leaves. A failed commit aborts the
process rather than acknowledge a write the file does not hold. Boot rebuilds
the maps from the file and refuses a format-version mismatch. redb keeps an
exclusive lock: one broker per volume, stop-copy backup, Recreate-only
rollouts. Measured on the dev box with 1.5 KB entities: about 3,100 fsynced
writes per second, commit p50 0.21 ms, p99 0.85 ms.

## Choosing the temporal driver

`ANTARES_TEMPORAL` names the history backend and defaults to following
`ANTARES_STORE`. Any pairing works: `file` current state with `timescale`
history, `postgres` current state with `memory` history. A backend different
from the store builds a second store instance used only for history; a
Postgres half runs its own maintenance and retention job wherever the
current state lives, and `/q/health` reports both halves (`store`,
`temporal`).

`ANTARES_TEMPORAL=none` installs the `NoTemporal` driver: `supported()` is
false, the recorder and the bookkeeping paths become no-ops, and every
client-facing temporal read answers `OperationNotSupported` with status 422
(CIM 009 Table 6.3.2-1):

```json
{
  "type": "https://uri.etsi.org/ngsi-ld/errors/OperationNotSupported",
  "title": "OperationNotSupported",
  "status": 422,
  "detail": "no temporal store is configured"
}
```

## What enters history

History is fed after the response, from a queue of `TemporalEvent`s the
entity endpoints produce. An event enters history only when every gate admits
it (`crates/antares-api/src/history.rs`):

1. **Value change**, in the producer: an attribute instance whose value did
   not change never becomes an event.
2. **`ANTARES_TEMPORAL_RECORD`**: `all` (the default) admits everything;
   `observed` keeps only instances carrying `observedAt`, so a write without
   one updates current state and leaves no history; `none` records nothing
   from the entity endpoints while the temporal API itself still stores what
   it is given; `ruled` (T-5056) admits what `all` does, but only for an
   Entity a retention rule opts in with a history duration.
3. **New observation**, in the Postgres and TimescaleDB stores
   (`ANTARES_TEMPORAL_DEDUP`, default on): an instance with an `observedAt`
   is one row per (Attribute, datasetId, observedAt), and a new value at the
   same instant corrects that row in place; an instance without one is
   written only when its value differs from the newest stored instance. Gate
   1 compares with the current entity; this one compares with history, so an
   upsert that recreates an Attribute with an unchanged value adds no row.
   Unchanged values are not repeated in the temporal representation
   (CIM 009 4.5: a static Property is one instance). `off` writes a row per
   admitted event, as before T-4507.

`observed` is a narrowing, not a tidying: an Entity created through the Core
API with no `observedAt` has no temporal evolution at all under it, and a
temporal query over such an Entity answers empty rather than answering with
its values. That is why `all` is the default — five of the ETSI temporal
conformance tests create their fixtures without an `observedAt` and expect to
read the history back. Choose `observed` only where every producer stamps its
own observation times.

A drain that fails leaves the client's 2xx standing and increments
`temporalDrainErrors` on `/q/health`. `ANTARES_TEMPORAL_RETENTION_DAYS` prunes
attribute instances older than the window from the maintenance job on the
temporal half; unset keeps everything. The migration sets no retention on
purpose: a schema that silently drops data is the wrong surprise.

## Measured storage cost

Measured on a 3.9 KB expanded entity (1.5 KB compact),
and on attribute instances of the same shape:

| Where | Per entity | Per attribute instance |
|---|---|---|
| Postgres, plain | 3.1 KB, 938 B of it the whole-document GIN index (dropped by 0019) | 1,387 B |
| TimescaleDB after columnstore compression | as plain | 120 B, read latency unchanged |
| Process memory (`serde_json::Value`, all modes) | 37.6 KB resident | as the entity |

Two consequences shape the design. The in-memory `Value` costs nine times its
text, so memory-mode capacity is bounded by RAM, not by the file. And
expanded JSON-LD costs 4.8 times the compact text but only 1.8 times on disk,
because Postgres compression absorbs the repeated IRIs; storing the compact
form would buy nothing.

### The instanceId lives in its column

An instance's `instanceId` is also the `instance_id` column of its history
row. Since T-5131 a new row leaves it out of `data` (about 70 B of a
245 B document for a bike count). Rows written before keep it there and read
back as stored: a row whose `data` lacks `instanceId` is one written since.
The reads aggregate each attribute's `instance_id` column beside its
documents, in the same order, and the store puts the member back into each
instance before the answer leaves it, so the API answers stay byte-identical.
Building the member into each row in SQL instead cost Postgres 10-35 % more. Copies into `attr_versions` keep whole documents, as before.

`createdAt`, `modifiedAt`, `observedAt` and `type` stay in `data`. Rebuilding
the two broker stamps from `created_at` and `modified_at` would save another
~75 B a row, but printing two timestamps per row cost Postgres 20-45 % more
on a whole-history read of 240,000 rows, whatever the encoding (`to_char`,
`timestamp`, epoch). The API echoes a client's `observedAt` spelling, and no
column holds `type`.

A rollback to a release older than T-5131 serves the rows written since then
without `instanceId` in temporal answers (the column still holds it, nothing
is lost); rolling forward again restores the answers.

### Integer history keys: the design (T-5193)

Slice 1 built (T-5200, migration 0020): the tables, the dictionary
lookup, every read over both parents and the maintenance of both. The
write routing and the cut-over (T-5243) and the dev measurement (T-5244)
follow. A history row keys on three texts: the Entity id (~60 B),
the full expanded Attribute IRI (~70 B) and the broker's
`urn:ngsi-ld:Instance:<uuid>` (57 B). Each is stored twice, in the heap and
in `u_attr_instances_time`. The plan replaces them with an `entity_no
bigint` and an `attr_no integer` from a per-Tenant dictionary of full
expanded IRIs, and with an `instance_no uuid`.

The columns cannot change type in place: `ALTER TYPE` rewrites every
partition of dev's ~110 GB history under an exclusive lock, and the
partitions of one parent must all have its columns. Two shapes were
measured on one synthetic week (2,000,000 rows: 2,000 docking stations, 4
Attributes, 250 instances each, identical `data` of 336 B, PostgreSQL 17,
`VACUUM ANALYZE`d, bytes per row):

| Shape | Heap | Index | Total |
|---|---|---|---|
| Today: text keys | 630 | 306 | 936 |
| B: a second parent `attr_rows` with integer and uuid keys | 455 | 90 | 546 |
| C: today's table, three nullable columns, the text keys NULL on new rows | 455 | 100 | 555 |

Reads, warm, both alternated twice: every row of the week through one
index scan per (Entity, Attribute), 4.5-5.9 s today, 4.4-5.1 s for B; the
newest 10 per (Entity, Attribute), 355-389 ms today, 276-287 ms for B.

Decided (worker-t5193, owner may override): shape B, because it needs no
work on the existing partitions. C needs a unique index on the new columns
of every existing partition, built one `CREATE INDEX CONCURRENTLY` a
partition as 0017 does, which reads the whole 110 GB for an index that
holds nothing there. C also turns the three text columns nullable on the
partitions that keep them, and its `CHECK` that a row holds exactly one key
shape would scan every partition too. B's new parent carries `NOT NULL`
keys from its first row.

The shape, as it is to be built:

- `iri_ids` (`tenant_id`, `id bigint` identity, `iri`): primary key
  (`tenant_id`, `id`), unique (`tenant_id`, `iri`), under the same
  per-command RLS policies as `attr_instances` (0005). Every lookup and
  every join carries the Tenant, so an id never names another Tenant's IRI.
- `attr_rows`: the columns of `attr_instances` with `entity_no`, `attr_no`
  and `instance_no` in place of the three texts, partitioned (or a
  hypertable) the same way, with the unique key (`tenant_id`, `entity_no`,
  `attr_no`, `observed_at`, `instance_no`).
- The cut-over is the Monday after the migration, stored in a one-row
  table the broker reads at boot. A row whose `observed_at` falls before
  it goes to `attr_instances` in the text shape, a late write included,
  so one instance has one conflict key. The partition job creates the weeks
  from the cut-over in `attr_rows` only. Retention drops weeks of both
  parents; once `attr_instances` holds no week, the store stops reading it.
- `instance_no` is the uuid of the broker's own `urn:ngsi-ld:Instance:<uuid>`
  in its canonical lowercase form. Any other id a temporal write brings
  (5.6.11.4) gets its uuid v5 as the key and stays in `data` as
  `instanceId`, which the read path since T-5131 already returns as stored.
  A v5 collision could only merge two instances that one writer created on
  one Attribute at one `observedAt`.
- The store keeps the dictionary per Tenant in memory. A write looks an IRI
  up and only on its first sight inserts it (`ON CONFLICT DO NOTHING`, then
  a read for the id another pod won). A read knows the ids from the request
  (the Entity ids of the page, the expanded Attribute names) and maps the
  answer's ids back from the same map, so a warm process sends no extra
  statement.
- Each history statement reads both parents as `UNION ALL`, one arm per
  parent, each arm through its own unique index. An ordered read with a
  `LIMIT` (the newest instance a write compares with, lastN's N-th newest)
  limits each arm itself: PostgreSQL merges two arms in index order only
  then, because an arm's equality on (`tenant_id`, `entity_no`, `attr_no`)
  does not make those leading index columns redundant in the parent's
  ordering, and it sorts every row of the Attribute otherwise. Copies into
  `attr_versions` keep whole documents, as now.
- Until the store keeps the dictionary in memory (T-5243), an arm binds
  its ids with `iri_no(tenant, iri)`, a STABLE invoker's-rights SQL
  function the planner evaluates once per index scan. `attr_rows_of(tenant,
  entity)` and `attr_rows_at(tenant, entity, attribute)` return `attr_rows`
  in the row type of `attr_instances`; the planner inlines them, so the
  caller's predicates reach the btree. An instance id is the broker's
  `urn:ngsi-ld:Instance:<uuid>` unless `data` holds another `instanceId`.
- A release older than the change reads only `attr_instances` and would
  miss every row since the cut-over, so the migration that sets the
  cut-over is `breaking_` (`migrations.rollback.toml`). 0020 leaves it NULL
  and is `compatible`: until it is set every row goes to `attr_instances`.

The keys save about 390 B a row. Getting a dev row under 300 B also needs
the `data` side; today only TimescaleDB's columnstore gets there (120 B).

## Migrations

`crates/antares-sql/migrations/`, applied by the first process to start
unless `ANTARES_MIGRATE=0` hands the job to an init container.

| Migration | Adds |
|---|---|
| `0001_init` | The PostGIS and `btree_gin` extensions; `tenants`; `entities` with the GiST location index, the `jsonb_path_ops` GIN serving `q=`, the tenant-scoped type index and the expiry index; `subscriptions`, `csource_subscriptions`, `csource_registrations` with the `csource_index` match table; `jsonld_contexts`; `entity_maps` and `entity_map_docs` (5.5.9.3 distributed pagination); `outbox`; `snapshots`; `dist_subs`; `temporal_entities`; `maintenance_jobs`; `attr_instances` as a hypertable when TimescaleDB is present, otherwise range-partitioned with a default partition, plus the lookup index and the idempotent-upsert key `(tenant_id, entity_id, attr_id, instance_id, observed_at)`. Row-Level Security with a `tenant_isolation` policy on every tenant-scoped table. |
| `0002_dead_letters` | `dead_letters` (`tenant_id`, `id`, `doc`), same RLS belt, read through the [admin API](admin-api.md#dead-letters). |
| `0003_comma_seconds_fraction` | Rewrites `try_timestamptz` to accept the comma decimal separator 4.6.3 permits in requests. The stamps live in `jsonb` as the client wrote them, and a NULL from this function means "no expiry" to `NOT_EXPIRED` and to the 4.22 instance reap — so a comma-stamped `expiresAt` made the document immortal instead of raising. |
| `0004_drop_entity_maps` | Drops the `entity_maps` row store `0001_init` created. EntityMaps (5.14) are documents in `entity_map_docs`, which is what every read and write path uses; nothing ever wrote a row to the table. |
| `0005_service_escape_by_command` | Splits the `tenant_isolation` policy on `entities`, `outbox` and (plain mode) `attr_instances` into one policy per command. `0001_init` wrote a single `FOR ALL` policy whose `USING` clause named the `antares.service` escape, and `USING` is what PostgreSQL applies to the existing row of an `UPDATE`: a role that armed the escape could move another tenant's row into its own, because the `WITH CHECK` only sees the new row. The escape now reaches `SELECT` and `DELETE`, which is all the outbox drain and the two 4.22 reaps use. |
| `0006_context_tenant` | Gives `jsonld_contexts` a `tenant_id` column and the RLS belt every other tenant-bearing table carries (ADR-0021). The column is `GENERATED` from the row: `NULL` for a `Cached` copy of a public document, which belongs to no tenant and is readable by all of them, and otherwise the `owner` member the row already carried — the default tenant for a row written before that member existed. No `antares.service` escape: nothing about a stored `@context` is cross-tenant work. |
| `0007_outbox_claim_check` | Gives `outbox` a `published_at` stamp and the two partial indexes that keep the drain's page and the reap off each other's rows. An event whose bodies exceed the bus message ceiling travels as a claim-check reference, and this row is the only remaining copy of what it references — the store's current row is the after-image, so the before-image the notification diff needs is not recoverable from it. The drain stamps such a row published instead of deleting it, the consumer reads the event back by `seq`, and the maintenance pass frees it 24 hours later. |
| `0008_views` | `views` (`tenant_id`, `id`, `doc`), same RLS belt: the endpoint views of a `views` build (ADR-0023 rule 10), one document per view under the reserved tenant `antares-views`, written through `/q/views` and read by every pod's reload. A vanilla broker creates the table and never writes it. |
| `0009_dataset_instances` | `temporal_entities.dataset_attrs text[]`: the Attributes of the Entity that hold an instance with a datasetId, kept by the instance write. A temporal lastN reads an Attribute through its newest rows only when it is not listed (T-4246). A column with a constant default is a catalog change; a partial index on `attr_instances` would have read every partition under a lock that blocks INSERT (T-4310). |
| `0010_history_retention_index` | A partial index on `entities (tenant_id)` for the Entities that carry their own `historyRetention` (DM-97): the retention sweep reads them across every Tenant once a minute without scanning current state. Building it is one scan of `entities`, never of the history. |
| `0011_versions_provenance` | `attr_instances.version` and `.provenance` (constant defaults, a catalog change on every partition) and `attr_versions`, the superseded versions of a corrected measurement under the same per-command RLS as `attr_instances`; a correction copies the old row there and rewrites the current one with the next version, so no history read changes (T-4665, ADR-N-054 2.1, 2.3, 2.4). |
| `0012_entity_types` | `entity_types` (`tenant_id`, `type`), the type catalogue, same RLS belt (T-4683). A trigger on `entities` adds each type an insert, or an update that changes `types`, carries; `ON CONFLICT DO NOTHING` leaves a listed type's row unlocked, so writers of one type never queue on it. `/types` and MCP `list_types` read the names here and keep a name while a live Entity carries it, instead of a `DISTINCT` over every Entity: `i_entities_first_type` (`tenant_id`, `types[1]`) answers a type some Entity carries first by a skip scan, one probe per type, and `i_entities_types_multi`, a GIN over the Entities of two or more types alone, answers a type carried only beside another and a view's "beside" probe. The migration backfills from `entities` in one scan. `0011` belongs to T-4665's versions; sqlx applies whichever lands second. |
| `0013_type_page_indexes` | The type page's indexes (T-4781). `types @> ARRAY[…]` is no index condition under RLS (`arraycontains` is not leakproof), so the store reads one named type as two leakproof arms, each ordered by `id`: `types[1] = T` through `i_entities_first_type_id` (`tenant_id`, `types[1]`, `id`) and the Entities of two or more types that carry it later through the partial `i_entities_multi_type_id` (`tenant_id`, `id`). Both include `types` and `expires_at`, so count=true is an index-only scan. The first index replaces `i_entities_first_type`, its prefix, which the type catalogue's skip scan reads through the new one. |
| `0014_attrs_expire_at` | `entities.attrs_expire_at`, the earliest `expiresAt` of any Attribute instance in `entity` (4.22, T-4837), written by every entity write beside `expires_at`, with the partial index `i_entities_attrs_expire`. The instance reap reads the rows due by it, rewrites each Tenant's under that Tenant (0005 grants the service escape no UPDATE) with the expired instances pruned and `location` recomputed, `modified_at` untouched, then deletes the history rows, in one transaction; at most 1000 rows a tick. The migration backfills per Tenant. |
| `0015_rls_index_paths` | Index paths the broker's role can use under RLS (T-4852). No PostGIS, jsonb or array operator is leakproof, so the GIST on `location` and the GIN on `temporal_entities.types` never served a request and every geoquery and temporal `type=` read its whole Tenant. `entities.location_cell` (generated, `text COLLATE "C"`) holds the geohash cell of the whole bounding box of `location` (`''` when no cell does or PostGIS refuses it), indexed (`tenant_id`, `location_cell`); a geoquery ANDs a cover of its region over it. `temporal_entities` gets the first-type (`tenant_id`, `types[1]`) and partial multi-type (`tenant_id`) indexes `entities` got in 0013. `csource_index` gets (`tenant_id`, `registration_id`), the key of its foreign key and of each registration update's delete, which read the whole table per registration. Adding the generated column rewrites `entities` once. |
| `0016_entity_type_statistics` | Extended statistics `entities_tenant_types` (MCV over `tenant_id`, `types[1]`, `cardinality(types)`), filled by ANALYZE (T-4852). The planner treated a Tenant and its types as independent and estimated a tenth or less of a Tenant's Entities of one type, so a type page sorted a bitmap of the whole type instead of reading `i_entities_first_type_id` in id order. Changes no access and no result. |
| `0017_history_one_index` | One unique btree `u_attr_instances_time` (`tenant_id`, `entity_id`, `attr_id`, `observed_at`, `instance_id`) replaces `i_attr_instances_lookup` and `u_attr_instances` (T-5057): it serves the per-attribute read backwards and the upsert's conflict, and saves one index entry a history row (~150 B on a fresh build, 9 GB of a 30 GB week on dev). An empty history swaps in the migration; a plain-mode history with rows is moved by the maintenance job, one partition a pass with `CREATE INDEX CONCURRENTLY`, then one short transaction attaches them and drops the two old indexes. Until then each partition carries one more index on disk. A timescale hypertable with history keeps the two old indexes (timescale has no concurrent unique build). |
| `0018_typed_location_paths` | Two btrees the broker's role reaches under RLS (T-5136): `i_entities_type_location_cell` (`tenant_id`, `types[1]`, `location_cell`) serves a geoquery of one type, whose cover over 0015's index read every type's rows in the cells (17 000-47 000 rows for a page of 20 on the distinct-1000 rig); `i_entities_type_unlocated` (`tenant_id`, `types[1]`, `id`) `WHERE location IS NULL` serves `!location` (an OGC `bbox` page's Features without a geometry), to which the store ANDs `location IS NULL`; the negated `@?` alone read the whole type for no row. Changes no access and no result. |
| `0019_drop_entities_jsonb` | Drops `i_entities_jsonb`, the `jsonb_path_ops` GIN over the whole document (T-5193). No jsonb operator is leakproof (`@?`, `@>`, `?|`), so under forced RLS the broker's role never used it as an index condition: as `antares_app`, a q= page of 200,000 Entities read the primary key and filtered 199,998 rows with the GIN in place, while a superuser's plan read the GIN. Every Entity write still paid a GIN entry per path. 1,869 -> 1,754 B an Entity on that synthetic set; a deployment that bypasses RLS loses the GIN path its q= pages used. |
| `0020_history_integer_keys` | The second history parent of the integer keys (T-5200, "Integer history keys" above): `iri_ids` (per-Tenant dictionary of full expanded IRIs, 0005's per-command RLS, no UPDATE policy), `attr_rows` (partitioned or a hypertable as `attr_instances`, unique key (`tenant_id`, `entity_no`, `attr_no`, `observed_at`, `instance_no`), its RLS and compression as 0001 gives `attr_instances`), the one-row `history_cutover` holding NULL, and the functions `iri_no`, `attr_rows_of` and `attr_rows_at`. Every history read and delete covers both parents; nothing writes `attr_rows` while the cut-over is NULL. |
| `0023_entity_attrs` | `entity_attrs` (`tenant_id`, `type`, `attr`), the attribute catalogue, same RLS belt (T-4934). A trigger on `entities` adds each (type, member) pair an insert carries, and on an update only the pairs it adds (a new member or a new type), so a write that keeps the Entity's shape inserts nothing; `ON CONFLICT DO NOTHING` leaves listed pairs unlocked. `/types?details=true` and `/attributes` read the pairs for each live type, take the first page of Entities the caller sees and probe only the names that page lacks and the view can show, instead of folding the Tenant: 16-17 ms for 100,000 Entities locally, where the keyset fold took about 2 s. A removed member keeps its pair, which then costs one walk of its type per call. The migration backfills from `entities` in one scan. `0021`-`0022` belong to T-5349. |
