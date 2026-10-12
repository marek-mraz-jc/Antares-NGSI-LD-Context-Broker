# Surfaces threat model, module rules, code rules and supply chain (T-3447)

Sources: Antares-NGSI-LD-Context-Broker `62a566b` (branch `agent/worker-2/T-3436`; `git diff --stat 7185e1a..62a566b -- crates Cargo.toml dev .github` is empty, the commits since touch `docs/` only, so every code line cited here is also the code of `7185e1a`, the commit the `cg` graph was built at, `cg schema`: "built 2026-10-10T11:32:43Z @ 7185e1a100"). joinedcontext-platform was not read directly; its rows are cited through `docs/analysis/gateway-to-broker-map.md` (T-3436, platform `59a8f70`) and `docs/analysis/mcp-surface.md` (T-3442, read at 463 lines at `62a566b`). ETSI TS 104 175 V1.1.1 and TS 104 176 V1.1.1 under `/workspace/etsi-specs/`. crates.io API and `static.crates.io` tarballs read on 2026-10-10 (grep only, nothing built). Task T-3447; binds `tasks/GOAL-antares-surfaces.md` points 5, 6 and 8.

Inputs: `docs/analysis/operation-matrix.md` + `.yaml` (T-3437, "the matrix"), `docs/adr/ADR-0023-endpoint-views.md` (T-3438, its rules are cited as "ADR-0023 rule N"), `docs/analysis/gateway-to-broker-map.md` (T-3436, its security rows are cited as "map S1" to "map S26"), `docs/adr/ADR-0014-extension-hooks-model.md`, `docs/adr/ADR-0020-policy-seam.md`, `docs/analysis/odata-library.md` (T-3448 survey). `docs/analysis/odata-linkml-mapping.md` (T-3441, cited as "T-3441" with its section or row id: its `L1` to `L13` are LinkML path rows, its `W`/`Q`/`F` ids are OData rows, its `S1` to `S5` security items) and `docs/adr/ADR-0024-odata-library.md` (T-3448) appeared at `62a566b` while this was written and were read; the OData and LinkML rows below follow them. The LinkML threat rows of this page are numbered `LK1` to `LK8` so they cannot be confused with T-3441's path rows.

## 1. Scope, method and how to read the tables

Six surfaces, one section each (3 to 8): the NGSI-LD view, OData, MCP, LinkML, WebSocket notifications and internal (in-process) Context Source registrations. Section 2 holds what every surface shares: the trust boundaries, the assets, the bounds that already exist, the findings in today's code that every surface inherits, and the cross-surface STRIDE rows. Section 9 is the `module-rules` text T-3443 adopts verbatim. Sections 10 and 11 are the code rules and the supply chain.

STRIDE tables have five columns: `id`, `S/T/R/I/D/E` (Spoofing, Tampering, Repudiation, Information disclosure, Denial of service, Elevation of privilege; several letters when one threat is several), `threat`, `mitigation`, `test`. Test naming:

- An existing test is cited as `file::fn` with the file relative to `crates/antares-api/tests/` unless a path is given; every one was checked with `sed -n` on 2026-10-10 to be a `#[tokio::test]` or `#[test]`.
- A proposed test is marked "(proposed)". ADR-0023 already names the `views_*` tests of its rules (its Confirmation table); those names are reused unchanged and marked "(proposed, ADR-0023)". Proposed files follow the pattern of the existing ones: `crates/antares-api/tests/views_*.rs` (ADR-0023 puts them there), `crates/antares-odata/tests/odata_*.rs`, `crates/odata-*/tests/*.rs` for the protocol libraries, `crates/antares-mcp/tests/mcp_*.rs`, `crates/antares-linkml/tests/linkml_*.rs`, `crates/antares-ws/tests/ws_*.rs`, `crates/antares-icsr/tests/icsr_*.rs`, `crates/antares-ops/tests/ops_*.rs`. A proposed name of this page names the file it goes into, in the form `file.rs::fn`. Names taken from T-3441 (`docs/analysis/odata-linkml-mapping.md` section 10) and T-3442 (`docs/analysis/mcp-surface.md` section 10) are marked "(proposed, T-3441)" or "(proposed, T-3442)"; where the source gave no file, T-3444 assigns one.
- A row whose mitigation is a CI script names the script and the workflow step, and the script's `--self-test` is its test.

GOAL point 5 cases and where they are answered: inference through `q` on hidden attributes (V1, V2, V4, V9, O1), count and paging leaks (V3, O3, O4), out-of-view joins (V8, V9, O5), notification timing leaks (V14, W5). The extra cases of the task: request smuggling through `$batch` (O8 to O12), resource exhaustion by body size (G4, O11, MC6, W3), nesting depth (G4, O7, O11), `$expand` fan-out (O6), WebSocket connections (W2), MCP output size (MC5); cache poisoning of `$metadata` across views (O16, and LK2, MC9 for the same class on the other surfaces); confused deputy through internal CSR (C1 to C3).

## 2. What every surface shares

### 2.1 Trust boundaries

| # | boundary | who is on each side | what crosses it |
|---|---|---|---|
| B0 | gateway to broker | the platform gateway (authenticates, rate limits) and the broker | the request, the subject headers named by `ANTARES_POLICY_SUBJECT_HEADERS` (ADR-0020: "opaque to the broker — it parses no credential and validates no token") |
| B1 | surface crate to operation layer | the surface's protocol code and `antares-ops` (section 9.1) | one TS 104 175 operation per call, the caller's headers |
| B2 | operation layer to store | the NGSI-LD handlers behind `gate!` (`crates/antares-api/src/lib.rs:27`) and the store drivers | queries already narrowed by the seam |
| B3 | broker to egress | notification delivery, federation forwards | notifications after `pre_notify`, forwards stripped of subject headers (ADR-0020 "The subject never travels") |
| B4 | tenant to tenant | two tenants of one broker | only through a Context Source registration (ADR-0011) or, in section 8, an internal one |
| B5 | admin to broker | `/q`, which writes the view table (ADR-0023 rule 10) | configuration; outside the seam by ADR-0020 ("`/q` is outside the seam: the admin surface is the gateway's to protect") |

### 2.2 Assets

A1 hidden Entities (out of a view); A2 hidden Attribute values; A3 hidden Attribute and type names; A4 counts and paging state; A5 the tenant name (map S22); A6 the subject identity (ADR-0020); A7 the view configuration and the existence of other views; A8 broker availability (memory, connections, delivery slots); A9 integrity of stored Entities; A10 the conformance claim of the vanilla binary (GOAL point 1).

### 2.3 Bounds that exist today and apply to every surface

Every registered surface sits behind the same bounds wall as the NGSI-LD root: `crates/antares-api/src/lib.rs:487-498` nests each `ApiSurface` and layers `DefaultBodyLimit::max(*bounds::MAX_BODY_BYTES)` and `bounds::bounds_layer` over all of them ("a deployment's own /x routes are no less a way in"), proven by `api_surface.rs::a_registered_surface_is_behind_the_bounds_wall`. An inner `AppState::call` request passes the wall again (`facade_seam_f0.rs::an_inner_request_is_counted_by_the_bounds_wall`).

| constant | `crates/antares-api/src/bounds.rs` | value (source) | answer | used by rows |
|---|---|---|---|---|
| `MAX_BODY_BYTES` | 21 | 4 MiB default, `ANTARES_MAX_BODY_BYTES` (:21-27) | bare 413 | G4, O11, MC6, W3 |
| `MAX_URI_BYTES` | 28 | 8 KiB | bare 414 | O7 |
| `MAX_JSON_DEPTH` | 29 | 64 | 400 BadRequestData | G4, O11, MC6, W3 |
| `MAX_GEO_VERTICES` | 37 | 1024 (`crates/antares-ql/src/geo.rs:29`) | 400 | V23, O7 |
| `MAX_BATCH_ITEMS` | 42 | 1000 default, `ANTARES_MAX_BATCH_ITEMS` | 400 | O11 |
| `MAX_FED_RESPONSE_BYTES` | 54 | 16 MiB default | peer part skipped, warning | C5 |
| `MAX_FED_INFLIGHT` / `FED_INFLIGHT` | 65 / 72 | 256 default | callers wait | C5 |
| `MAX_FED_FANOUT` | 77 | 8 default | concurrency per request | C5 |
| `MAX_PEER_WARNINGS` | 92 | 8 | warnings truncated | C10 |
| `MAX_IN_PROCESS_CALL_DEPTH` | 107 | 8 | 500 InternalError | G2, C4 |
| `MAX_JOIN_LEVEL` | 109 | 10 | 400 | V8, O6 |
| `MAX_CONTEXT_FETCHES` | 114 | 32 (`crates/antares-jsonld/src/loader.rs:614`) | 400 | V23 |
| `MAX_Q_LINK_LOOKUPS` | 116 | 512 (`crates/antares-ql/src/eval.rs:17`) | budget per `q` | V9, O6 |
| `MAX_REGEX_CACHE`, `_BYTES`, `MAX_REGEX_PROGRAM_BYTES` | 119 | 1024, 64 MiB, 256 KiB (`crates/antares-ql/src/regex.rs:50-58`) | 400 | V23 |
| `MAX_Q_NODES` | 122 | 512 (`crates/antares-ql/src/lib.rs:266`) | 403 TooComplexQuery | V23, O7 |
| `MAX_FOLD_DOCS` | 132 | 100 000 default, `ANTARES_DISCOVERY_SCAN_MAX` | 403 TooManyResults | V7, O21, LK4 |
| `CHANGE_QUEUE` | 151 | 1024 | batch dropped and counted | W4 |
| `DELIVERY_WIDTH` / `_PER_TENANT` | 160 / 180 | 64 / 8 defaults | queued | W4 |
| `MAX_TRACKED_DESTINATIONS` | 204 | 4096 | LRU drop | W4 |
| policy `TIMEOUT` | `crates/antares-api/src/policy.rs:40` | 250 ms default, `ANTARES_POLICY_TIMEOUT_MS` | Deny | G3 |

Note on clause numbers: `bounds.rs` comments cite CIM 009 "6.3.4" for 411, 413 and 414. In TS 104 176 V1.1.1 the 413 and 411 rules are in `ts-104176-ngsi-ld-http-binding/6/6.3.2.md`: "'Request Entity too large' (413) which shall be raised when the HTTP input data stream provided by a client was too large i.e. too many bytes" and "'Length required' (411) ...". TS 104 176 6.3.4 is now "Expiration time for cached @contexts". The comment drift is recorded for T-3461 (re-audit of changed clauses); no 414 rule was found in TS 104 176 clause 6 (grep for `414`: no hit).

Every new cap a surface adds (proposed names in sections 3 to 8) is a `bounds.rs` constant or `LazyLock` knob, published in `LimitStats::snapshot` (`bounds.rs:215-243`), so the existing `bounds.rs::tests::every_declared_cap_is_published` (`bounds.rs:612`) and `the_documented_health_limits_are_the_published_ones` (`bounds.rs:423`) cover it. Values are chosen by the implementing task and the T-3443 ADR, not here.

### 2.4 Findings in today's code that every surface inherits

| # | finding | evidence | consequence | answered by |
|---|---|---|---|---|
| F1 | `AppState` exposes the store: `pub store: Arc<dyn CurrentStateDriver>` and `pub temporal` (`crates/antares-api/src/state.rs:78`, `:81`), and `ApiSurface::router(&self, st: AppState)` hands a surface the whole state (`crates/antares-api/src/surface.rs:28`). The reference surface already reads the store past the seam: `antares_plugin_example::surface::count` references `antares_api::state::AppState::store` at line 56 and calls `CurrentStateDriver::list` (`examples/plugin-example/src/surface.rs:51-60`) | cg, Cypher below | a surface built the same way answers without `gate!`, without a view and without `pre_notify` | G1, module rule M2 |
| F2 | `AppState::call` copies `NGSILD-Tenant`, `NGSILD-Snapshot`, `Link` and every subject header from the caller only "when it does not set them itself" (`state.rs:604-622`: `if parts.headers.contains_key(name) { continue; }`) | code | a façade or an internal forward that writes its own tenant or subject header is judged under that value: the confused-deputy path of section 8 | G2, C3 |
| F3 | the JSON depth scan runs only when `Content-Type` is absent or contains `json` (`bounds.rs:344-356`); a `multipart/mixed` OData `$batch` passes the wall unscanned | code | per-part JSON bodies need their own depth check | O11 |
| F4 | `echo_tenant_layer` echoes `NGSILD-Tenant` only when the caller sent it (`lib.rs:693-711`); an inner `AppState::call` response carries the tenant the façade set | code | a view or façade response can name the pinned tenant to a caller who never sent it (map S22) | V18 |
| F5 | fixed by T-3475: the per-commit `cargo deny` in `.github/workflows/workspace.yml` runs `check all` (licenses, advisories, bans, sources) and `deny.toml` bans `openssl-sys`, `native-tls`, `aws-lc-sys`; before it ran `check licenses advisories` only and `bans`/`sources` waited for the weekly `advisories.yml` | workflow | a new git or unknown-registry source, or a banned crate, is red per commit | section 11 |
| F6 | `[profile.release] strip = "symbols"` (`Cargo.toml:144`), and `full.yml` strips the release binary again | Cargo.toml, `full.yml` "Build + package" | a symbol check cannot read the release binary; it needs an unstripped build | module rule M3 |
| F7 | clippy runs `cargo clippy --locked --workspace --all-targets -- -D warnings`, without `--all-features` (`workspace.yml:150`) | workflow | code under `#[cfg(feature = "...")]` in `antares-broker` and `antares-api` is not linted; GOAL point 8 asks for `--all-features` | section 10 |
| F8 | tungstenite 0.29.0 (the version `antares-ws` takes through `tokio-tungstenite 0.29`) defaults to `max_write_buffer_size: usize::MAX`, `max_message_size: Some(64 << 20)`, `max_frame_size: Some(16 << 20)` (`tungstenite-0.29.0/src/protocol/mod.rs:100-102`) | crate tarball | an unbounded write buffer per slow WebSocket client and a 64 MiB message above the 4 MiB body cap | W3, W4 |
| F9 | the workspace clippy table sets only `unwrap_used = "warn"` and allows `needless_borrows_for_generic_args` (`Cargo.toml:162-166`); every crate inherits it with `[lints] workspace = true` | Cargo.toml | the GOAL point 8 denies are not in force anywhere | section 10 |

```cypher
// F1: who outside antares-api touches AppState::store (7 rows; the plugin surface is one)
MATCH (s:Symbol)-[r:REFERENCES]->(f:Symbol)
WHERE f.kind='Field' AND f.name='store' AND f.path CONTAINS 'AppState' AND s.package <> 'antares-api'
RETURN s.package, s.path, r.line;
// F1: the plugin crate's direct store calls outside tests
MATCH (s:Symbol)-[r:CALLS|REFERENCES]->(t:Symbol)
WHERE s.package='antares-plugin-example' AND t.package IN ['antares-store','antares-sql'] AND NOT s.is_test
RETURN s.path, t.path;
```

### 2.5 Cross-surface STRIDE rows

| id | S/T/R/I/D/E | threat | mitigation | test |
|---|---|---|---|---|
| G1 | E, I | a surface reads or writes the store directly (F1) and skips `gate!`, the view and `pre_notify` | surfaces get an `Ops` handle from `antares-ops`, never `AppState`; `Ops` has no store field and no `pub` accessor to one; module rule M2 makes a store edge red in CI | `dev/check-module-rules.py --self-test` (proposed, section 9); `crates/antares-ops/src/lib.rs` doc test `Ops` with a `compile_fail` block that names `ops.store` (proposed) |
| G2 | S, E | a façade sets its own `NGSILD-Tenant` or subject header on the inner request (F2) and is judged under it | `antares-ops` builds every inner request itself: its request type has no field for the tenant, the snapshot or a subject header; it copies them from the outer request and overwrites any value a surface put into a free-form header list | `crates/antares-ops/tests/ops_run.rs::a_surface_cannot_set_tenant_snapshot_or_subject`; `facade_seam_f0.rs::the_handle_carries_the_headers_that_decide_what_is_served` |
| G3 | E | an operation reached through a surface is not gated, or gated twice | every surface operation is one `AppState::call` into a route that already calls `gate!` once; the route walk is extended to every surface route | `policy_gate_seam.rs::every_route_asks_the_policy_engine_exactly_once`; `operation_matrix.rs::every_cell_is_reachable_or_rejected_as_stated` (every NGSI-LD route on the root and through the view root under views `all` and `restricted`: a served cell asks once with its row's clause, any other is refused before the engine); `operation_matrix.rs::every_route_of_every_surface_is_a_matrix_cell` |
| G4 | D | body size and JSON nesting on a surface route | the bounds wall over every surface (`lib.rs:487-498`) | `api_surface.rs::a_registered_surface_is_behind_the_bounds_wall`; `facade_seam_f0.rs::an_inner_request_is_counted_by_the_bounds_wall`; `security_regression.rs::oversized_body_is_rejected_before_any_parse` |
| G5 | E | a surface mounts under `/ngsi-ld/v1` or `/ex` and changes the conformance surface | `check_prefix` (`surface.rs:42-50`) and `overlaps` (`:55-57`) refuse it at startup | `api_surface.rs::a_surface_cannot_mount_outside_the_reserved_prefixes`; `surface.rs::tests::only_the_reserved_prefixes_are_mountable` |
| G6 | E | a surface feature is compiled in but serves without a view, or is linked into the vanilla binary | module rules M3 and M4 | `dev/check-vanilla-symbols.sh --self-test` (proposed); `crates/antares-broker/tests/surfaces_fail_closed.rs::a_selected_surface_without_a_view_is_closed` |
| G7 | S | a client reaches the broker without the gateway and sends a subject header itself (map S2, S26) | outside the broker: the NetworkPolicy admits only the gateway and the Portal (map S26); inside the broker the subject is only the headers the deployment named | `policy_gate_seam.rs::the_subject_carries_no_header_the_deployment_did_not_name`; deployment side: a conftest rule in joinedcontext-deployment that the broker's NetworkPolicy admits no other pod (proposed, outside this repository) |
| G8 | D | a façade over a façade recurses until the stack ends | `MAX_IN_PROCESS_CALL_DEPTH` (`bounds.rs:107`) checked in `AppState::call` (`state.rs:582-590`) | `facade_seam_f0.rs::a_facade_that_calls_itself_is_ended_by_the_broker_not_by_the_stack` |
| G9 | I | an error body leaks a hidden name, another tenant's id or the store's own text (map S23) | error mapping of section 10.3: details are fixed strings or echo the caller's own input; a 5xx detail from an inner call is replaced | `crates/antares-ops/tests/ops_errors.rs::an_inner_5xx_detail_never_reaches_the_surface` (proposed) |
| G10 | R | a write through a surface cannot be traced to the surface and view that made it | every operation a surface runs writes one `surface operation` log line with the surface name, the view id and Tenant, the operation and its status, never the subject or an argument (ADR-0020). Decided (worker-2, owner may override, T-3593): the line and not the change event, because the in-process change queue's one reader is the matcher, and a bus=nats outbox row is written inside the store's transaction, where no surface is known; options: the log line (chosen), a member on the change tuple, a `__via` member in the event's documents (one a matcher or notification could leak) | `ops_log.rs::a_surface_operation_is_logged_with_the_surface_and_the_view`; `policy_subject_on_the_wire_p6.rs::a_change_event_carries_no_subject` |

## 3. NGSI-LD view (`views`)

Entry points: every route of `router()` (`crates/antares-api/src/lib.rs:315-460`; 68 method and path pairs, matrix section 7.2) under `/x/views/{view}/ngsi-ld/v1/` (ADR-0023 rule 10), `HEAD` and `OPTIONS` on each; notifications of Subscriptions created through a view (egress, B3); the view table, written through `/q` (B5) and loaded at start and on a change-bus event.

Trust boundaries: B0, B1 (the view router calls `antares-ops` with the view id), B2, B3, B5.

Assets: A1 to A7, A9.

The threat table of ADR-0023 is the starting point; its rows are V1, V3, V5, V6, V7, V8, V10, V14, V15, V16, V17 and V19 here, with the rule that answers each. The other rows are new.

| id | S/T/R/I/D/E | threat | mitigation | test |
|---|---|---|---|---|
| V1 | I | inference through `q` on a hidden Attribute (`q=salary>50000` selects a subset) | ADR-0023 rule 2: a hidden term is evaluated as if the Entity did not contain it | `views_a_hidden_attribute_does_not_exist_for_q` (proposed, ADR-0023); `views_props.rs::q_over_hidden_equals_q_over_absent` (proposed property test: for a generated `q` and Entity, the answer with the Attribute hidden equals the answer with it removed) |
| V2 | I | timing or plan side channel: the store's plan or index use depends on the hidden values | rule 2 rewrites the hidden term to the constant `false` (its negation `true`) on the AST before the store sees it | `views_q.rs::a_hidden_term_never_reaches_the_store` (every answer through the view is the same whatever the hidden values are, on the entity, batch and temporal paths) |
| V3 | I | count and paging leak (`count=true&limit=0`, `Link` pages) | rule 1: conjunction before paging, ordering and count | `views_count_and_paging_describe_the_narrowed_set` (proposed, ADR-0023) |
| V4 | I | ordering leak (`orderBy=hidden`) | rule 2: ordered as if absent (7.6.2.2 puts "Attribute does not exist" last) | `views_q.rs::order_by_a_hidden_attribute_orders_as_absent` (proposed) |
| V5 | I | enumeration of out-of-view Entities by id | rule 3: 404 ResourceNotFound, the answer of an absent id | `views_an_out_of_view_entity_is_not_found` (proposed, ADR-0023); `policy_filter_5_7.rs::a_hidden_entity_is_a_resource_not_found`; `tenant_isolation.rs::cross_tenant_probe_is_indistinguishable_from_nonexistence` |
| V6 | I | existence oracle on create: 409 for an id that exists out of view | accepted residual, decided in the matrix (section 8, item 4) and ADR-0023 rule 3: 409 as TS 104 175 10.2.2.4 fixes it; the 409 body names only the id the caller sent | `views_writes.rs::a_create_of_an_out_of_view_id_reveals_only_that_the_id_is_taken` (the 409 body equals the 409 body of an in-view id with the id replaced) |
| V7 | I | discovery leak: `/types`, `/attributes`, `@context` terms | rule 5 | `views_discovery_is_computed_from_the_view` (proposed, ADR-0023) |
| V8 | I | join leak: `join=inline` embeds an out-of-view target | rule 6, the shared visibility function T-3470 adds | `views_join_omits_out_of_view_targets_and_keeps_the_relationship` (proposed, ADR-0023); `policy_filter_5_7.rs::a_join_does_not_carry_a_narrowed_member_out_of_the_answer` (projection only, matrix section 8 item 1) |
| V9 | I | a linked `q` path (a `LinkedEntityRelation` term) is evaluated against an out-of-view target, so the caller learns a hidden Entity's value through a visible one | rule 2 at every hop: a hop into an Entity the view does not select is a non-existent target element; the hop budget stays `MAX_Q_LINK_LOOKUPS`. Not enforced today: the linked evaluation reads each target straight from the store (`crates/antares-api/src/entities.rs:1260-1265` into `notify::linked_eval`, `crates/antares-api/src/notify.rs:548`, store read at `:572`), filed by T-3441 as T-3474; until it is done a view refuses `q` terms with linked hops (400) | `views_q.rs::a_linked_q_path_does_not_cross_into_an_out_of_view_entity` (proposed); `join_budget_4_5_23.rs::the_lookup_ceiling_bounds_the_request_not_each_entity_on_the_page` |
| V10 | T | write laundering: a PATCH of `scope` moves an Entity into another view or out of sight | rule 7: pre-image visible, post-image still in view, else 403 | `views_writes_check_pre_and_post_image_and_the_allowlist` (ADR-0023) |
| V11 | T | a Replace Entity (10.2.10) through a view deletes the hidden Attributes it omits, because the caller cannot see them | Decided (worker-2, owner may override): a replace through a view whose stored Entity holds any hidden Attribute is refused with 403, because the matrix pre-image for `replace_entity` requires "every Attribute the stored Entity holds" to be writable (section 6) and a silent merge would not be a replace; options: refuse, or keep the hidden Attributes (a merge under another name) | `views_writes.rs::a_replace_is_refused_when_the_stored_entity_holds_hidden_attributes` |
| V12 | T, E | one decision for a whole batch lets an allowed member carry a forbidden one | rule 7: per member, `BatchOperationResult` per member, one batch-shaped engine call | `views_batch_is_decided_per_entity`; `views_writes.rs::views_batch_upsert_is_decided_per_entity` (ADR-0023, T-3594) |
| V13 | T, I | a Relationship write points at an out-of-view target; answering "target not visible" while accepting a non-existent target (NGSI-LD has no referential integrity) is an existence oracle | Decided (worker-2, owner may override): a Relationship write through a view needs the target to be visible in that view, and an absent target is refused the same way (403 with one fixed detail), because only then do "hidden" and "absent" answer alike; options: refuse both (chosen), check only visible-or-absent (an oracle), no check (a link into another view) | `views_writes.rs::a_relationship_target_must_be_visible_and_absent_answers_alike` |
| V14 | I | notification timing leak: a notification fires when only a hidden Attribute changed | rule 9: not sent, counts as not triggered | `views_notifications_drop_hidden_changes_and_strip_previous_value`, `views_subscriptions.rs::a_hidden_only_change_leaves_times_sent_unchanged`; `policy_notify_5_8_6.rs::a_dropped_notification_is_no_attempt_at_all` |
| V15 | I | notification content: `previousValue`, joined targets | rule 9 and rule 6 | `views_notifications_drop_hidden_changes_and_strip_previous_value`, `views_subscriptions.rs::views_notifications_join_only_what_the_view_shows`; `policy_notify_5_8_6.rs::a_filtered_notification_loses_only_what_the_engine_named` |
| V16 | I | temporal history from before the Entity entered the view | rule 8, documented residual, loader warning | `views_temporal_serves_current_members_and_limits_after_the_allowlist` (proposed, ADR-0023); `views_load.rs::a_temporal_view_keyed_on_a_mutable_property_is_warned` (proposed) |
| V17 | S, E | tenant switch: `NGSILD-Tenant` naming another tenant on a view path | rule 10: 400 BadRequestData | `views_pin_their_tenant` (proposed, ADR-0023) |
| V18 | I | the pinned tenant's name is echoed to a caller who never sent `NGSILD-Tenant` (F4, map S22) | the view router removes `NGSILD-Tenant` from the response unless the outer request carried it (then it is the same value, rule 10) | `views_tenant.rs::a_view_does_not_echo_a_tenant_the_client_did_not_send` (proposed); `internal_tenant_leaks.rs::a_snapshot_scoped_response_never_names_the_synthetic_tenant` |
| V19 | E | a view whose predicate failed to load is served wide | rule 10: not served, named in `/q/health`; `View` has no `Default` and a selector needs at least one type | `views_a_broken_view_is_not_served` (proposed, ADR-0023) |
| V20 | E | path smuggling of the view id: `/x/views/a/../b/`, `%2F` inside the id, `//`, a trailing dot | the view id is matched against `[a-z0-9-]{1,63}` (ADR-0023) on the raw, undecoded path segment before any normalisation; a percent-encoded `/` or `.` segment is 404 | `views_routing.rs::a_view_id_is_matched_on_the_raw_segment` (proposed) |
| V21 | I | `NGSILD-Snapshot` on a view path serves the frozen copy unnarrowed (the snapshot layer swaps the tenant, `lib.rs` snapshot layer) | the view predicate is applied on the snapshot read as on a live read; the gate resolves the snapshot tenant to its owner as ADR-0020 states | `views_tenant.rs::a_snapshot_read_through_a_view_is_narrowed` (proposed); `policy_maps_snapshots_5_14_5_16.rs::a_snapshot_scoped_read_is_judged_under_the_callers_tenant` |
| V22 | I | an EntityMap (14.x) built under view A is presented on view B or on the root, and pages through A's id set | Decided (worker-4, owner may override): no entity map is built or read through a view, a request naming one (`entityMap` or `NGSILD-EntityMap`) is 400, because a map holds one query's id set and binding it to a view needs a stored `__view` the root paths do not check; options: refuse maps through a view (chosen), store `__view` beside `__subject` (ADR-0020) and answer a map of another view as expired | `views_maps.rs::an_entity_map_is_bound_to_its_view`; `policy_maps_snapshots_5_14_5_16.rs::a_map_built_for_one_subject_is_not_reused_by_another` |
| V23 | D, I | resource exhaustion, and an oracle on the view's own size: if the view predicate's nodes were charged to the caller, a caller could learn the predicate's size by growing `q` until 403 TooComplexQuery | all existing caps apply (`MAX_BODY_BYTES`, `MAX_JSON_DEPTH`, `MAX_Q_NODES`, `MAX_GEO_VERTICES`, `MAX_JOIN_LEVEL`, `MAX_CONTEXT_FETCHES`, regex caps); Decided (worker-2, owner may override): `MAX_Q_NODES` counts the caller's `q` alone and the view predicate is bounded at load, because the caller's limit then does not depend on configuration it cannot see; options: count the caller alone (chosen), count the conjunction. Built (T-3570): every path parses the caller's text with `parse_q` before `Filter::narrow_params`, the narrowed text with `antares_ql::parse_narrowed_q`, whose caps are the caller's times the hidden-term rewrite (3) plus the conjunction plus the policy part; a view's `q` is held to `MAX_Q_NODES` at load (`antares_ops::views::parse_q`), and the joined policy `q` to its share by `antares_ql::fits_policy_q` on the tree alone, so a policy past it is the same AccessDenied for every caller. Across federation (T-3621): a narrowed `q` past `parse_q`'s caps is not forwarded (`federation::forwardable`), the local re-check applies it to the aggregate (5.7.2.4, 5.7.4.4), and an EntityMap forward (5.14.4, 5.14.5) asks the peer without it and keeps only the ids whose Entities, read back from that peer, it selects (`federation::narrow_map_ids`, T-3683), so a peer's TooComplexQuery or a missing peer cannot tell the caller the policy is there | `views_q.rs::the_q_node_cap_counts_the_callers_q_alone`; `policy_filter_5_7.rs::the_q_node_cap_counts_the_callers_q_alone`; `policy_filter_5_7.rs::an_engine_q_past_its_share_is_refused_whatever_the_caller_asks`; `policy_filter_5_7.rs::a_narrowed_q_past_a_peers_cap_answers_as_the_callers_q_alone`; `crates/antares-ql/src/lib.rs::complexity_tests::every_join_of_an_accepted_q_and_a_fitting_policy_parses`; `bounds.rs::tests::the_published_q_node_cap_is_the_enforced_one`; `security_regression.rs::one_batch_cannot_multiply_the_context_fetch_cap` |
| V24 | E | a Subscription created through a view is updated, read or deleted through another view or the root, or one from the root is listed in a view | the stored `__view` (rule 9) scopes 10.5.3 to 10.5.6: another view answers 404; the root, the Tenant's own surface, sees every Subscription (decided in T-3499) | `views_subscriptions.rs::a_subscription_is_reachable_only_through_its_view` |
| V25 | E | a view's operation allowlist is bypassed by a method the allowlist did not consider (`OPTIONS`, `HEAD`, a `DELETE /entities` variant) | rule 4: 405 with `Allow` computed from the allowlist, 422 when the method stays; `GET`/`HEAD` never removed from a served resource (TS 104 176 6.3.2) | `views_a_disallowed_operation_answers_405_with_allow` and `views_a_disallowed_operation_answers_422_when_the_method_stays` (proposed, ADR-0023) |
| V26 | T | a view change is applied on one pod and not another, so two pods serve two predicates | rule 10: the change bus swaps every pod's table atomically; a pod that missed an event reloads on the next one and reports the table revision in `/q/health` | `views_reload.rs::views_reload_without_restart`, `views_reload.rs::health_reports_the_view_table_revision` |
| V27 | E | the vanilla broker plus `AllowAll` behind a view that "narrows nothing" is mistaken for a full grant | a view with no selector is refused at load (rule 10); the ETSI run on an all-selecting view proves only conformance, not access | `views_load.rs::a_view_without_a_selector_is_refused` (proposed) |
| V28 | E, I | a view written to narrow widens to every type or Attribute: `*` beside a name, a vocabulary that defines a term `*`, a `*` rule beside named rules or with a condition (ADR-0023 rule 12) | `*` is read before any name is expanded and loads only as the whole-view form (selector `["*"]` and the one unconditioned rule `{types: ["*"], attributes: ["*"]}`); every other use is a load error; the whole-view form still carries the view's `q`, `scopeQ`, `geoQ`, `operations` and Tenant | `views_load.rs::star_loads_alone_and_nowhere_else`, `views_all.rs::views_all_answers_what_the_broker_answers`, `mcp_every.rs::every_name_is_offered_by_its_pattern` |

## 4. OData v4.01 (`odata`)

Entry points (matrix column `odata`, sections 5 and 7.1; OData 4.01 Protocol, URL Conventions and JSON Format): the service document and `$metadata` (mapped to 10.6.3 and 10.6.4 under the view, matrix section 7.2); entity sets and single entities (10.4.3, 10.4.2); `$count`; create (10.2.2, Protocol 11.4.2; deep insert is rejected with 501 in round 1, T-3441 W3); `POST .../$query` (T-3441 Q22, run as `GET /entities`); `PATCH`/`PUT`/`DELETE` of entities and properties; `$ref` and `@odata.bind` (10.2.3); `$batch` in the multipart (Protocol 11.7.7) and the JSON form (JSON Format 19.1). System query options `$filter`, `$select`, `$expand` (with `$levels`), `$orderby`, `$top`, `$skip`, `$count`, `$skiptoken`, `$format`. Headers `OData-Version`, `OData-MaxVersion`, `Prefer`, `If-Match`, `If-None-Match`, `Content-ID`. Refused with 501 before any gate: the 16 rows of matrix section 7.1 (`$apply`, `$search`, `$compute`, actions, functions, `$crossjoin`, `$all`, `$each`, delta update, media and stream, atomicity groups, upsert by key, derived type casts, ...).

Trust boundaries: B0; B1 between the protocol crates (`odata-url`, `odata-csdl`, `odata-json`, `odata-batch`, `odata-axum`, ADR-0024 decision 1, which know nothing about Antares; and `linkml-model`, which `antares-odata` also reads, T-3441 section 9) and `antares-odata`, which translates to operations; B1 again from `antares-odata` to `antares-ops`.

Assets: A1 to A5, A7 to A9.

Every OData request resolves exactly one view: the service root is `/x/views/{view}/odata/v4` (T-3441 section 11, ADR-0023 rule 10); a request that resolves no view is 404 and touches no gate. T-3441 section 10 (S1 to S5) is the mapping's own security list; each of its items is a row below, with its test names where it named one.

| id | S/T/R/I/D/E | threat | mitigation | test |
|---|---|---|---|---|
| O1 | I | `$filter`, `$select` or `$orderby` naming a hidden property tells "hidden" from "absent"; bisection of a hidden value | T-3441 S3: a hidden attribute is not in the model, so the option fails resolution with the 400 a misspelled name gets, the error names the option and never the value; below the surface ADR-0023 rule 2 holds as defence in depth | `odata_filter_on_hidden_attribute_equals_filter_on_unknown_name`; `odata_bisecting_a_hidden_value_learns_nothing`; `odata_orderby_hidden_is_400_like_unknown` |
| O2 | I, E, D | `$filter` translated to `q` by string concatenation regroups the view's terms (the precedence trap ADR-0020 describes); a filter that passes parsing but explodes after translation | translation from the `odata-url` AST to the `antares-ql` AST, then the ADR-0020 conjunction on the AST; T-3441 S2: OData limits (`max_expr_depth = 32`, `max_expr_nodes = 256`, ...) before translation and the broker's `MAX_Q_NODES` on the translated tree; over a bound: 400 `TooComplexQuery` | `odata_filter_ne_doubling_is_checked_on_the_translated_tree`; `filter_props.rs::odata_filter_translation_agrees_with_reference_eval` (property test); fuzz target `odata_filter_to_qnode` (proposed, T-3441) |
| O3 | I | `$count`, `@odata.count`, `$top=0&$count=true` describe the unnarrowed set | ADR-0023 rule 1: count computed by the store on the conjoined query (T-3441 S3) | `odata_count_is_the_narrowed_count` |
| O4 | I, T | `@odata.nextLink` / `$skiptoken` replayed on another view, edited to jump, or carried to another query | T-3441 S5.2: the token holds the offset, the remaining `$top` and a SHA-256 digest of the normalised query plus the view id; a mismatch is 400; with only an offset inside, a forged token can do no more than `$skip`, and every page is narrowed by the store again (ADR-0023 rule 1) | `odata_view.rs::odata_skiptoken_from_another_query_is_400`; `odata_view.rs::odata_a_skiptoken_of_another_view_is_400` |
| O5 | I | `$expand` to an out-of-view target, or to a navigation property whose target type the view does not serve | the navigation property is absent from `$metadata` when the target type is out of view; an out-of-view target instance is not embedded (ADR-0023 rule 6, one shared visibility function with `join`) and a single-valued navigation reads as `null`, as for an absent target | `odata_expand.rs::odata_expand_omits_out_of_view_targets` |
| O6 | D | `$expand` fan-out: depth, items per level, `$levels=max`, and the product of `$top` and expansions | T-3441 S1: depth at most `ExpandRestrictions/MaxLevels`, at most `MAX_JOIN_LEVEL` (`bounds.rs:109`), `$levels=max` clamped and a larger explicit value 400; `max_expand_depth = 3` and `max_expand_items = 8` (T-3441 S1 decision); Entities embedded in one answer at most `MAX_JOIN_LOOKUPS` = 1000 (`crates/antares-api/src/repr.rs:728`, the join walk's width budget); no expansion runs before T-3470. Decided (worker-2, owner may override): reaching `MAX_JOIN_LOOKUPS` fails the OData request with 400 naming the way to narrow it, because a partial expansion is silent truncation in a format whose clients cannot see an `NGSILD-Warning`; options: 400 (chosen), stop and annotate | `odata_expand.rs::odata_expand_stops_at_the_join_lookup_budget` (asserts the 400); `odata_expand_levels_above_max_is_400`, `odata_expand_max_is_clamped_to_max_levels` (proposed, T-3441: `$levels` answers 501 until then); `join_budget_4_5_23.rs::the_post_query_spends_one_allowance_across_its_page` |
| O7 | D | URL parser exhaustion: deep `commonExpr`, many nodes, long literals, long `in` lists | `odata_url::Limits` (`max_expr_depth`, `max_expr_nodes`, `max_literal_len`, `max_in_list_len`, `max_query_len`, ...; odata-library.md 3.1), checked arithmetic, `LimitExceeded` before allocating; `MAX_URI_BYTES` before the parser runs | `crates/odata-url/tests/limits.rs::every_limit_is_refused_before_it_allocates` (proposed); fuzz target `fuzz/fuzz_targets/odata_url_parse.rs` (proposed); the OASIS `odata-abnf-testcases.yaml` run (proposed, T-3510; odata-library.md section 4) |
| O8 | T, E | `$batch` request smuggling: a part addresses another host, another view, `/q`, `/ex`, `/ngsi-ld/v1` or escapes with `..`; a batch inside a batch | each part URL is resolved against this view's service root (all three forms Protocol 11.7.7.1 requires); the resolved path must stay below the service root and an absolute URI or `Host` must name this service, else the whole batch is 400 before any part runs (odata-library.md 3.4, T-3441 S5.1); a nested batch is 400 (ADR-0024 decision 5) | `odata_batch.rs::odata_batch_member_for_another_host_is_400`; `odata_batch.rs::odata_batch_refuses_a_part_outside_this_service_root` (another view, `/q`, `/ex`, the NGSI-LD root, `..`, percent-encoded `.` and `/`) |
| O9 | S, E | a `$batch` part carries `NGSILD-Tenant`, `NGSILD-Snapshot`, a subject header, `Authorization`, `Link` or `Prefer: ngsi-ld=...` and is judged under it | part header allowlist (`Content-Type`, `Content-ID`, `Content-Transfer-Encoding`, `Content-Length`, `Accept`, `If-Match`, `If-None-Match`, `Prefer` with `return` and `odata.*` preferences only, `OData-Version`, `OData-MaxVersion`); any other header is 400 for the whole batch; a member runs with the outer request's headers and its allowed ones over them, so tenant and subject come from the outer request through `Ops` (G2) | `odata_batch.rs::odata_batch_part_headers_cannot_change_tenant_or_subject` (both formats; no member runs); `odata_batch.rs::odata_batch_members_are_gated_one_by_one` (every decision under the outer tenant and subject) |
| O10 | T | multipart framing desync: a boundary inside a body, LF-only line ends, a third multipart level, a part `Content-Length` that disagrees with its bytes, duplicate `Content-ID` | `odata-batch` own parser over a bounded slice: exactly two levels, a framing line end of CRLF or a bare LF (a lone CR refused; a bare LF before the boundary is a delimiter, so the writer refuses a body holding one; T-3726), duplicate or unknown ids refused, a `dependsOn` names only an earlier member, so cycles and forward references are refused (odata-library.md 3.4; T-3505) | `crates/odata-batch/tests/multipart.rs::ambiguous_framing_is_refused` and `bare_lf_framing_reads_and_bodies_keep_their_bytes`; fuzz targets `fuzz/fuzz_targets/odata_batch_multipart.rs` and `odata_batch_json.rs` |
| O11 | D | `$batch` size: total bytes, part count, per-part JSON depth (F3: a `multipart/mixed` body is not depth-scanned) | total body under `MAX_BODY_BYTES` (wall); `max_requests` not above `MAX_BATCH_ITEMS`; every part whose type is JSON runs a depth scan against 16 (`odata-batch` `Limits::strict`, below `MAX_JSON_DEPTH`) before any part runs | `odata_batch.rs::odata_batch_part_bodies_are_depth_checked`; `odata_batch.rs::odata_batch_part_count_is_capped` |
| O12 | E | a dependent part (`dependsOn`, a `$1` Content-ID reference) inherits the decision of the part it depends on | Content-ID references are 501 in round 1 (ADR-0024 decision 5, T-3441 W26); `dependsOn` only orders; every part is its own operation through `Ops` and is gated alone (T-3441 S5.1) | `odata_batch.rs::odata_batch_members_are_gated_one_by_one` (a recording engine sees as many decisions as the members sent alone); `odata_batch.rs::odata_batch_content_id_reference_is_501` |
| O13 | T | atomicity groups promise all-or-nothing the broker cannot give | refused with 501 (matrix 7.1 `odata_batch_atomicity`) | `odata_refused.rs::odata_refused_features_answer_501_before_any_gate` (iterates every refusing OData row of matrix 7.1 in `operation-matrix.yaml`) |
| O14 | T | deep insert writes some Entities and fails on a later gate (OData demands all-or-nothing, Protocol 11.4.2.2; NGSI-LD has no transaction, matrix section 8 item 7) | rejected with 501 in round 1 (T-3441 W3, ADR-0024 decision 5); when it is offered, every gate is decided before the first write | `odata_write.rs::odata_deep_insert_is_501` |
| O15 | T, I | `@odata.bind`, `$ref` or a referential-constraint Property links to an out-of-view or absent target | V13: the target is read under the view first (T-3441 section 6, S5.4); collection `$ref` add/remove is 501 (T-3441 section 11) | `odata_write.rs::odata_bind_to_hidden_or_missing_target_is_400_and_writes_nothing` (a hidden and an absent target, on a create and a merge, the same 400, T-3760); `odata_write.rs::odata_a_reference_to_a_hidden_or_missing_entity_is_400_and_writes_nothing` (a Property keying the target, mapping R6: hidden, absent and of another type the same 400 on every write form, T-3761); `odata_ref.rs::odata_bind_to_an_absent_target_answers_like_a_hidden_one` (`$ref` writes) |
| O16 | I, T | `$metadata` cache poisoning across views or subjects: a document built for view A, or for a subject whose engine probes differ, is served for another | T-3441 7.3 cache key: tenant, view id and revision, the engine probe bits, schema version, `@context` hash, OData version; a bounded LRU of 256 models per pod; probes are not cached. T-3441 7.4: strong `ETag` = SHA-256 of the bytes served, `Cache-Control: no-cache`, `If-None-Match` 304. Decided (worker-2, owner may override): `Cache-Control: private, no-cache`, amending T-3441 7.4, because the bytes depend on per-subject engine probes and a shared cache must not store one subject's document at all; options: `private, no-cache` (chosen), `no-cache` alone | `odata_metadata.rs::metadata_etag_changes_when_the_view_changes`; `odata_metadata.rs::odata_metadata_is_cached_per_view_and_subject_and_never_served_across_them` (two views of one tenant and two subjects of one view; no document carries a name the other may not see; the engine probe bits join the key with section 4's Capabilities) |
| O17 | I | `$metadata` lists hidden types, properties or navigation properties | T-3441 S4: the model is built only from `selector ∩ engine` and `allowlist ∩ engine`; a Relationship to a hidden type becomes a string property; one `Model` serves XML, JSON and request validation | `metadata_never_names_a_hidden_type_or_attribute`, `metadata_relationship_to_hidden_type_is_a_url_string`, `metadata_xml_and_json_describe_one_model` |
| O18 | I, T | `If-Match`/`If-None-Match` or an `ETag` tells an out-of-view Entity apart, or a check-then-write races | T-3441 7.4: no entity `ETag` in round 1 and `If-Match`/`If-None-Match` on writes answered 501 (T-3441 W32) before any operation, for any key, so the answer is the same for a visible, a hidden and an absent Entity and no precondition is ever read | `odata_write.rs::odata_a_conditional_write_or_one_answered_as_xml_is_refused` |
| O19 | E | a refused feature (action, function, `$crossjoin`, `$all`) reaches a gate or the store | 501 before any gate (matrix 7.1) | `odata_refused.rs::odata_refused_features_answer_501_before_any_gate` |
| O20 | I | an OData error message carries a hidden name, an inner ProblemDetails `detail` or store text | mapping of section 10.3 | `odata_errors.rs::odata_errors_carry_no_hidden_name_and_no_inner_detail` |
| O21 | D | `$metadata` built per request over a large tenant | built once per view revision, bounded by `MAX_FOLD_DOCS` (discovery folds) | `odata_metadata.rs::odata_metadata_is_built_once_per_view_revision` |
| O22 | T | a request body (`odata-json`) with too many properties, nested entities or bind links | `odata_json::BodyLimits` (`max_depth`, `max_properties`, `max_nested_entities`, `max_bind_links`, `max_collection_len`; odata-library.md 3.3) | `crates/odata-json/tests/limits.rs::every_body_limit_is_refused` (proposed); fuzz target `fuzz/fuzz_targets/odata_json_body.rs` (proposed) |
| O23 | R | an OData write cannot be traced to the OData surface | G10 | `ops_seam.rs::a_change_event_names_the_surface_and_the_view` (proposed, T-3593: G10) |
| O24 | E | `POST .../$query` carries the query in a body the engine never sees (the matrix 5.1 gap of `query_entities_post`) | T-3441 S5.3: `/$query` runs as `GET /entities` with `q`, never as `entityOperations/query` | `odata_query.rs::odata_query_segment_reaches_the_engine_with_q` |
| O25 | E | Capabilities annotations in `$metadata` read as a grant by a client or by the surface itself | T-3441 S5.5: annotations are advisory; every request is gated | `odata_capabilities.rs::odata_capabilities_true_but_engine_denies_is_403` |
| O26 | I | a `$filter` navigation path hops into an out-of-view target (T-3474: the linked `q` evaluation reads each target from the store without `view ∧ engine`, `entities.rs:1260-1265`, `notify.rs:548-572`) | navigation paths in `$filter` stayed 501 until T-3474; since it, a single-valued hop is the `q` term `rel{Type:attr}` under `join=inline&joinLevel=1` (5.7.2.4) with `attrs` naming the structural members only, and the broker resolves each target through `view ∧ engine`, a hidden one like a dangling link (T-3508) | `odata_expand.rs::odata_filter_navigation_path_reads_hidden_targets_as_absent` |
| O27 | S, T | a client's `Host` header names another host in the context URL, `@odata.nextLink`, `@odata.id` and `Location` it is answered with (T-3688: they are absolute, as Microsoft.OData.Core requires) | `ANTARES_PUBLIC_URL` fixes the root a deployment answers with; without it the root is `http://{Host}`, the fallback the JSON-LD context URL already uses, and only the client that sent the header reads it (`$metadata` is `private`) | `reads.rs::the_context_and_next_links_are_absolute_under_the_requests_host` |

## 5. MCP (`mcp`)

Entry points (`docs/analysis/mcp-surface.md`, T-3442 draft, read at 463 lines; its decisions are cited as "MCP D-n" and its security rules as "MCP rule n" from its sections 1 and 10): `POST /x/views/{view}/mcp` (MCP 6.3), JSON-RPC methods `server/discover`, `tools/list`, `tools/call` (32 tools, MCP D-1), `resources/list`, `resources/templates/list`, `resources/read`, `subscriptions/listen` (MCP D-7); the legacy `initialize` handshake for 2025-11-25 and 2025-06-18 clients (MCP D-8); headers `MCP-Protocol-Version`, `Mcp-Method`, `Mcp-Name`, `Origin`, `Host`. Protocol library: `rmcp` 3.5.1 (notes of 2026-10-10: `LATEST = V_2026_07_28`, `LATEST_WITH_INITIALIZE = V_2025_11_25`), which knows nothing about Antares.

Trust boundaries: B0 (the gateway stays the OAuth resource server, MCP D-9); B1 from `rmcp` into `antares-mcp` and from `antares-mcp` into `antares-ops`; B3 for resource-update streams; and a boundary the other surfaces lack: the LLM client, which turns tool output into its next input (prompt injection through stored data).

Assets: A1 to A7, A8 (long-lived listen streams), A9.

Module-rule note: MCP rule 2 says "The `antares-mcp` crate depends on `antares-api` and `rmcp`". Under module rule M2 (section 9) the edge is to `antares-ops`, not to `antares-api`; the test MCP rule 2 names is kept. `mcp-surface.md` rule 2 was aligned in the same push. MCP 6.3 left "Whether `rmcp` contains `unsafe`" to this task: a grep of the 3.5.1 tarball's `src/` finds no line with the word `unsafe` and no `forbid(unsafe_code)` attribute (section 11.2).

| id | S/T/R/I/D/E | threat | mitigation | test |
|---|---|---|---|---|
| MC1 | I | `tools/list` or a call to a hidden tool reveals which operations a view or the engine hides | MCP D-3: the list is the allowlist filtered by one batch-shaped engine probe; a hidden and an unknown tool, resource or type answer byte-identically (MCP rule 3) | `mcp_discovery.rs::a_hidden_and_an_absent_name_answer_the_same_bytes_on_every_method` |
| MC2 | I | input-schema `enum`s list hidden types or Attributes | MCP D-4: enums from the view's discovery (ADR-0023 rule 5), filtered by a name pattern; the schema is a convenience, the gate enforces | `mcp_discovery.rs::input_schemas_list_only_visible_names` |
| MC3 | E | a tool executes without the gate, or reads the store | every tool and resource read is one `Ops` call into a gated route (MCP rule 2, G1, G3); Decided (worker-4, owner may override): the engine sees a call twice, its gate once and the tool list's probe once (an Operation that names nothing), because without the probe a tool the engine hides would answer 403 where an unknown one answers `-32602` (MC1) | `mcp_gate.rs::every_tool_and_resource_asks_the_policy_engine_exactly_once`; `mcp_gate.rs::a_denied_operation_is_a_tool_error_and_never_an_empty_result` |
| MC4 | T, S | a write without the person's confirmation, a forged or replayed `requestState` | MCP D-11 and rule 10: HMAC-SHA256 `requestState` over tool, argument digest, subject digest, view, tenant, expiry and nonce, single use; the confirmation runs after the gate, never instead of it | `mcp_writes.rs::a_tampered_request_state_is_refused`, `mcp_writes.rs::a_replayed_request_state_is_refused`, `mcp_writes.rs::a_request_state_for_one_view_is_refused_on_another`, `mcp_writes.rs::a_write_the_engine_denies_never_asks_the_person`, `mcp_writes.rs::a_confirmed_write_the_gate_denies_is_refused` |
| MC5 | D, I | MCP output size: an unbounded tool result; the platform reads up to 8 MiB into `null` and the assistant cuts at 20 000 characters (MCP section 8) | MCP D-10: `MCP_MAX_RESULT_BYTES` per result, a shorter page with `nextCursor`, a single over-budget item is `isError` naming `pick`/`attrs`/`lastN`; never truncation | `mcp_output.rs::a_large_result_is_paged_never_truncated`; `mcp_output.rs::one_item_past_the_budget_is_an_error_that_names_how_to_narrow`; one test per row of the MCP section 8 table at the bound and one past it (MCP rule 12) |
| MC6 | D | request size, JSON depth, a JSON-RPC batch array | `rmcp` `max_request_body_bytes` set to `MCP_MAX_REQUEST_BYTES` (MCP 6.3) under the broker's wall (`MAX_BODY_BYTES`, `MAX_JSON_DEPTH`, G4); Decided (worker-2, owner may override): a JSON-RPC array body is refused with `-32600`, because JSON-RPC batching was removed from MCP in revision 2025-06-18 (that changelog, not re-read today; whether 2026-07-28 restored it: unknown, nothing in the notes of 2026-10-10 says so) and one array would multiply every per-call bound | `mcp_transport.rs::a_json_rpc_batch_is_refused` (the surface refuses it before `rmcp`, which answers an array with a bare 415); `api_surface.rs::a_registered_surface_is_behind_the_bounds_wall` |
| MC7 | D | `subscriptions/listen` streams exhaust memory, connections or delivery slots | MCP section 8: 64 URIs per stream, 4 streams per subject per process, 1 h lifetime; plus a per-tenant and per-process cap (proposed `MAX_MCP_LISTEN_STREAMS`, `MAX_MCP_LISTEN_STREAMS_PER_TENANT`, published in `/q/health`); each URI is one internal Subscription and counts against the delivery width per tenant | `mcp_listen.rs::listen_streams_are_capped_per_subject_tenant_and_process`, `mcp_listen.rs::a_closed_stream_deletes_its_internal_subscriptions` |
| MC8 | S | DNS rebinding from a browser: `rmcp`'s `allowed_origins` default is empty, which "disables Origin validation", and `allowed_hosts` is localhost only (MCP 6.3) | allowed hosts and origins from the deployment, `enforce_origin_validation()` on; 2026-07-28 `basic/transports/streamable-http`: "Servers MUST validate the `Origin` header on all incoming connections to prevent DNS rebinding attacks" (quoted by MCP 6.3) | `mcp_transport.rs::a_foreign_origin_is_403` |
| MC9 | I | `CacheableResult` shared across subjects or views (2026-07-28 SEP-2549 `ttlMs` + `cacheScope`) | MCP D-6: `cacheScope: "private"` on every view-narrowed result, `"public"` only on `server/discover` | `mcp_cache.rs::every_cacheable_result_but_discover_is_private` |
| MC10 | I | `resources/read` of `ngsi-ld://{otherView}/...` or another tenant's id | the URI's view must be the endpoint's view; otherwise `-32602` like an unknown resource (2026-07-28 moved resource-not-found from `-32002` to `-32602`, notes of 2026-10-10) | `mcp_resources.rs::a_resource_of_another_view_answers_like_an_unknown_one` |
| MC11 | T, E | prompt injection: an Entity value shaped like an instruction steers the client; tool descriptions built from data | MCP rule 1: results are `structuredContent` plus the same JSON as text; descriptions and `instructions` are static strings; no value is interpolated into a description, error or elicitation message | `mcp_output.rs::an_entity_value_shaped_like_an_instruction_is_returned_as_json_and_appears_in_no_description_error_or_message` |
| MC12 | T, I | exfiltration through `create_subscription` to an address the model chose | MCP rule 7: endpoint URI allowlist `ANTARES_MCP_SUBSCRIPTION_ENDPOINTS`, empty by default; ADR-0010 egress guard; confirmation | `mcp_writes.rs::a_subscription_to_an_endpoint_outside_the_allowlist_is_refused_before_the_gate` |
| MC13 | E | downgrade to the legacy `initialize` era to reach tools or behaviour the modern era hides | MCP D-8: the legacy era has no sessions, no resource subscriptions and no write tools | `mcp_transport.rs::the_legacy_handshake_offers_no_write_tool` |
| MC14 | S, E | header and body disagree (`Mcp-Method`/`Mcp-Name`, SEP-2243): a gateway rule decides on the header while the broker executes the body | MCP rule 11: a mismatch is 400 before any dispatch | `mcp_transport.rs::a_header_that_disagrees_with_the_body_is_400_header_mismatch` |
| MC15 | S | a tool argument names a tenant, a space or a subject and is turned into a header | MCP rule 13: no argument becomes a header; `tenant`/`space` arguments refused; `Ops` has no such field (G2) | `mcp_gate.rs::a_tenant_or_subject_in_the_arguments_is_refused_and_changes_nothing` |
| MC16 | I | joins through a tool reach an out-of-view Entity | MCP rule 4: `join`/`joinLevel` were kept out of every tool schema until T-3470; T-3470 checks every join target against the view, so T-3513 offers them | `mcp_gate.rs::join_is_an_argument_now_that_the_target_is_checked` (renamed from `join_is_not_an_argument_until_the_target_is_checked`, T-3513) |
| MC17 | I | resource-update events leak hidden changes or out-of-view Entities | MCP rule 8 and ADR-0023 rule 9: events carry only the URI; hidden-only changes and out-of-view Entities trigger nothing | `mcp_listen.rs::a_change_to_a_hidden_attribute_sends_no_resource_update`, `mcp_listen.rs::a_stream_watches_the_visible_entities_it_names_and_nothing_else`, `mcp_listen.rs::a_subscription_naming_a_stream_it_did_not_open_is_refused` |
| MC18 | I | a JSON-RPC `error.data` or a tool error text carries an inner ProblemDetails `detail` or a hidden name | section 10.3; MCP rule 1: a refusal's text is status and `title` only | `mcp_errors.rs::errors_carry_no_hidden_name_and_no_inner_detail` |
| MC19 | R | a tool call cannot be traced | G10, plus one log line per call with view, tool, operation id and request id, never the argument values (they may hold personal data) | `mcp_log.rs::a_tool_call_is_logged_without_its_arguments` (a test binary of its own: tracing caches callsite interest per process) |
| MC20 | D | the cursor decoder is fed a crafted cursor | cursor at most 2048 characters (MCP section 8), decoded with bounded base64 and JSON; a cursor of another tool or arguments is refused | `mcp_output.rs::a_cursor_of_another_tool_is_refused`; fuzz target `fuzz/fuzz_targets/mcp_cursor.rs` |

## 6. LinkML schema surface (`linkml`)

Entry points: read-only `GET`/`HEAD` under `/x/views/{view}/linkml` (T-3441 section 8, path rows L1 to L13: `schema/index.json`, `schema/v{major}/model.linkml.yaml` and its JSON Schema, `@context`, SHACL, OWL/RDFS/Markdown/Data Cube renderings, `classes/{type}`, `slots/{attr}`); every write method 405 (T-3441 L11). The matrix column `linkml` offers no write row; the documents are built from 10.6.4, 10.6.5 and 10.6.8 under the view, gate clause 5.7.6 for L1 to L7 (T-3441 section 8). Protocol library: `linkml-model` (T-3441 section 9 decision), no `antares-*` dependency; it reads the resolved LinkML JSON of the view's data model with `serde_json` and `#[serde(deny_unknown_fields)]` at view load, and writes YAML with a deterministic emitter that double-quotes every scalar, "so no YAML library is linked and no YAML is parsed" (T-3441 section 8). So the request path parses nothing but the URL; the one parser is the load-time JSON reader.

Trust boundaries: B0, B1. Assets: A3, A5, A7, A8.

| id | S/T/R/I/D/E | threat | mitigation | test |
|---|---|---|---|---|
| LK1 | I | the schema lists hidden types, Attributes, `is_a` parents, enum values or ranges that point at out-of-view types | rendered from the intersected model, never from a committed file (T-3441 section 8); `is_a` only between visible classes, a Relationship range to a hidden class becomes `range: uriorcurie`, `inverse` only when both ends are visible (T-3441 L2); until ADR-0023 rule 5 is implemented, an engine `Filter` on 5.7.6 is answered 501 (T-3441 section 8 decision) | `crates/antares-linkml/tests/linkml_view.rs::linkml_prefixes_and_context_hold_only_visible_terms` (L1 to L9: the YAML, every formalism and the index); `crates/antares-linkml/tests/linkml_view.rs::linkml_errors_carry_no_hidden_name`; `crates/antares-linkml/tests/linkml_view.rs::linkml_a_relationship_to_a_hidden_class_is_uriorcurie`; `crates/antares-linkml/tests/linkml_view.rs::linkml_a_denied_class_or_slot_is_the_404_of_an_absent_one` |
| LK2 | I, T | schema cache poisoning across views or subjects (as O16) | the O16 key and LRU; strong `ETag` of the rendered bytes, `If-None-Match` 304 (T-3441 L12); `Cache-Control: private, no-cache` (the O16 decision) | `linkml_view.rs::linkml_schema_is_cached_per_view_and_subject_and_never_served_across_them` |
| LK3 | T | YAML injection through names: an IRI or term holding `:`, `#`, a newline, `&`/`*` anchors or `!!` tags changes the document's structure | the emitter double-quotes and escapes every scalar (T-3441 section 8); a name with a control character is refused at view load, not at render | `crates/linkml-model/tests/emit.rs::names_are_quoted_and_cannot_inject_yaml`; `crates/linkml-model/tests/props.rs::emitted_yaml_parses_back_to_the_same_model` (property test, `serde-saphyr` as a dev-dependency only) |
| LK4 | D, T | building the schema per request over a large tenant; a crafted or oversized LinkML JSON at load (unknown metaslots, depth) | built once per cache key, folds bounded by `MAX_FOLD_DOCS`; the load-time reader uses `deny_unknown_fields` (a view with an unknown metaslot is not served, ADR-0023 rule 10) and the `json_depth` scan against `MAX_JSON_DEPTH` before `serde_json` runs | `crates/antares-linkml/src/render.rs::linkml_schema_is_built_once_per_cache_key`; `crates/linkml-model/tests/read.rs::an_unknown_metaslot_fails_the_load`; fuzz target `fuzz/fuzz_targets/linkml_read.rs` |
| LK5 | E | a write method on a schema path | 405 with `Allow: GET, HEAD` (T-3441 L11, ADR-0023 rule 4; TS 104 176 6.3.2: "a Context broker shall recognize the methods GET and HEAD on all endpoints") | `linkml_view.rs::linkml_is_read_only` |
| LK6 | I | prefixes or `@context` terms reveal the tenant's stored contexts, one of which may name a hidden Attribute (ADR-0023 rule 5) | the `@context` rendering (T-3441 L4) holds only the visible terms; prefixes only from the core context and the contexts the view names | `linkml_view.rs::linkml_prefixes_and_context_hold_only_visible_terms` |
| LK7 | D | an oversized schema document | a size bound, `ANTARES_SCHEMA_MAX_BYTES` (T-3628); Decided (worker-2, owner may override): the schema write past it is refused with `413`, so no oversized schema reaches the surface and the Tenant's views keep serving everything else, because the size is a property of the configuration and not of a request; options: refuse the write (chosen), close the surface at load, answer 403 TooManyResults per request | `linkml_view.rs::an_oversized_schema_is_refused_and_the_surface_stays_closed` |
| LK8 | I | an error body echoes a hidden name | section 10.3 (ProblemDetails) | `linkml_view.rs::linkml_errors_carry_no_hidden_name` |

## 7. WebSocket notifications (`ws`)

Entry points: an HTTP upgrade below `/x` (ADR-0003 names the crate `antares-ws` and the feature `ws`, "plugging into the NotificationSink scheme registry and a Router::merge — core crates never change"); client messages that create and delete Subscriptions whose endpoint is the connection; server messages that carry notifications. No WebSocket binding exists in the specifications (matrix section 2.4; ADR-0003: "a WS Notification Binding is an official NGSI-LD 2.1 work item"), so the message format is an Antares one and makes no conformance claim. Library: `tokio-tungstenite` 0.29 and `tungstenite` 0.29 on the connection hyper upgrades; the opening handshake (RFC 6455 section 4.2) is `antares-ws`'s own, never axum's `ws` feature, which cargo would unify into every build of the workspace's axum, the vanilla broker's included (T-3704; `dev/check-vanilla-symbols.sh --metadata` guards it) (F8).

Trust boundaries: B0 at upgrade time only (the gateway authenticates the upgrade, then the connection outlives the request); B1; B3 (delivery is the broker writing to the socket).

Assets: A1 to A3, A6, A8.

| id | S/T/R/I/D/E | threat | mitigation | test |
|---|---|---|---|---|
| W1 | S | cross-site WebSocket hijacking: a page in a browser opens the socket with the user's cookies | `Origin` checked against the deployment's allowlist at upgrade (the same knob as MC8); absent `Origin` is a non-browser client and passes | `crates/antares-ws/tests/ws/ws_bounds.rs::ws_a_foreign_origin_is_refused` |
| W2 | D | WebSocket connection exhaustion | caps per process and per tenant (proposed `MAX_WS_CONNECTIONS`, `MAX_WS_CONNECTIONS_PER_TENANT`), an idle timeout and a maximum lifetime (proposed `ANTARES_WS_MAX_LIFETIME_SECS`), all published in `/q/health`; over a cap the upgrade answers 503 and is counted | `ws_bounds.rs::ws_connections_are_capped_per_tenant_and_per_process` |
| W3 | D | message and frame size: tungstenite defaults to 64 MiB messages and 16 MiB frames (F8), above the 4 MiB body cap | `max_message_size` set to the body cap or less, `max_frame_size` set below it; every client message passes `json_depth` against `MAX_JSON_DEPTH` before its parse | `ws_bounds.rs::ws_an_oversized_or_too_deep_client_message_closes_the_connection` |
| W4 | D | a slow consumer: tungstenite's write buffer is unbounded by default (`max_write_buffer_size: usize::MAX`, F8), so a reader that never reads grows broker memory | a bounded per-connection queue; on overflow the connection is closed and counted, never buffered; a delivery slot (`DELIVERY_WIDTH_PER_TENANT`) is not held while a socket is full | `ws_queue.rs::ws_a_slow_consumer_is_disconnected_not_buffered` |
| W5 | I | notification content and timing leaks | delivery goes through the sink registry and `deliver_as`, so `pre_notify` runs under the subscriber (ADR-0020) and the view rules 6 and 9 apply | `ws_delivery.rs::ws_notifications_pass_pre_notify_under_the_subscriber`; `policy_notify_5_8_6.rs::the_notification_subject_is_the_subscribers` |
| W6 | S, I | subscription takeover: a client names another connection as its endpoint | the endpoint URI of a WebSocket Subscription is minted by the broker for the connection that created it and is not client-settable; a Subscription is delivered only on that connection | `ws_delivery.rs::ws_a_subscription_is_delivered_only_to_the_connection_that_created_it` |
| W7 | S | the subject captured at upgrade outlives the token the gateway checked | the connection ends at its maximum lifetime (W2), which a deployment sets at or below its token lifetime; the subject is the one stored with each Subscription (`__subject`, ADR-0020) | `ws_resume.rs::ws_a_connection_ends_at_its_max_lifetime` |
| W8 | E | a view is narrowed or removed while a connection still holds Subscriptions made through it | ADR-0023 rule 9: the view is applied at match time; a removed view stops delivery and closes its connections | `ws_views.rs::ws_a_removed_view_stops_delivery` |
| W9 | D | a flood of subscribe messages on one connection | a per-connection Subscription cap (proposed `MAX_WS_SUBSCRIPTIONS_PER_CONNECTION`); each create is an ordinary gated 10.5.2 through `Ops` | `ws_bounds.rs::ws_subscriptions_per_connection_are_capped` |
| W10 | R | connections cannot be traced | open and close logged with view, tenant, connection id and close reason; no subject value in the log (ADR-0020) | `ws_log.rs::ws_open_and_close_are_logged_without_the_subject` |
| W11 | T | a client message parser fed crafted input | bounded serde parse of a closed message enum with `deny_unknown_fields` | fuzz target `fuzz/fuzz_targets/ws_client_message.rs` |

## 8. Internal (in-process) Context Source registrations (`internal-csr`)

Entry points: Register, Update and Delete Context Source Registration (12.2.2 to 12.2.4) with a target that is another tenant of the same broker; every operation the matrix column `internal_csr` marks supported, forwarded by `AppState::call` instead of HTTP (matrix section 1: "the forward is `AppState::call` (`crates/antares-api/src/state.rs:571`) instead of HTTP"); csource notifications. Cross-tenant federation inside one broker exists today over HTTP: "a reader tenant reaches other tenants' data only through CSRs whose `tenant` member points back at this same broker" (ADR-0011, test `crates/antares-wasm/tests/handle.rs::csr_tenant_federates_across_tenants_in_one_broker`).

Trust boundaries: B4 above all; B1 (the forward is an `Ops` call); B3 for csource notifications.

Assets: A1, A2, A5, A6, A9 of the target tenant.

The confused deputy: the broker can read every tenant. A registrant in tenant A who registers tenant B as a Context Source asks the broker to read B on A's behalf, and today nothing in B has agreed to that: the registration is A's data (12.2.2), and with `AllowAll` the forward is judged by nobody. Over HTTP the gateway refuses Context Source registration through an Endpoint (map S16); in process there is no gateway between A and B.

| id | S/T/R/I/D/E | threat | mitigation | test |
|---|---|---|---|---|
| C1 | E | confused deputy: a registration in A naming tenant B makes the broker read B for A | Decided (worker-2, owner may override): an internal registration needs an export grant held by the target tenant (a record in B, written by B's own gated write or through `/q`, naming the source tenant, the view that narrows what B exports, and read or read-write), checked when the registration is created and again on every forward, because only B can consent to B's data leaving B; options: grant held by the target (chosen), trust the registrant (today's HTTP behaviour), admin-only registrations | `crates/antares-icsr/tests/icsr_grant.rs::icsr_a_registration_into_a_tenant_that_granted_nothing_is_refused`; `icsr_grant.rs::icsr_a_revoked_grant_stops_forwards`; `icsr_notify.rs::icsr_a_revoked_grant_stops_notifications` (proposed, T-3700) |
| C2 | E | the forward is judged under the registrant, the broker or the source tenant instead of the caller | matrix section 8 item 8: the source tenant's narrowing travels as `q`/`scopeQ` as on HTTP forwards, and the target tenant's engine is asked again about the caller; the grant's view narrows on top (ADR-0023: `effective = view ∧ engine`) | `icsr_gate.rs::icsr_the_target_engine_is_asked_about_the_caller`; `facade_seam_f0.rs::the_policy_engine_sees_the_caller_behind_the_facade`; `narrowing_through_proxies_5_7_2.rs::exactly_the_narrowed_entities_survive_a_registration_and_two_proxies` |
| C3 | S | the forward's tenant or subject header is set from registration data (F2: `AppState::call` keeps any value the inner request already carries, `state.rs:604-622`), or `contextSourceInfo` asks for it: TS 104 175 9.5.1: "the special value "urn:ngsi-ld:request" can be used to indicate that the respective value is to be taken from the request that triggered the given request" | `NGSILD-Tenant` of the forward comes only from the validated registration target; the subject headers are always overwritten from the caller and never taken from `contextSourceInfo` (ADR-0020 drops them from the 4.3.6.5 copy); 9.5.1 also says "As Tenant information, if applicable, is directly specified in the CSourceRegistration, it shall not be part of contextSourceInfo", so a tenant key in `contextSourceInfo` is refused | `icsr_gate.rs::icsr_the_subject_is_the_callers_and_never_comes_from_the_registration`; `policy_subject_on_the_wire_p6.rs::a_registration_cannot_ask_for_the_subject_header` |
| C4 | D | a cycle A to B to A in process | `MAX_IN_PROCESS_CALL_DEPTH` (`bounds.rs:107`) and the per-tenant Via alias (ADR-0011) | `facade_seam_f0.rs::a_facade_that_calls_itself_is_ended_by_the_broker_not_by_the_stack`; `federation_loop.rs::a_chain_naming_another_tenant_is_not_a_loop`; `icsr_loops.rs::icsr_a_two_tenant_cycle_ends_with_508`; `crates/antares-icsr/tests/icsr_depth.rs::icsr_past_the_depth_bound_no_forward_is_made` (each in-process forward on its own task: a chain at the bound polled frame inside frame overflowed a worker stack, T-3520) |
| C5 | D | fan-out amplification: one query forwards to many tenants | in-process forwards count against `MAX_FED_FANOUT` and `FED_INFLIGHT` exactly as HTTP forwards do | `icsr_loops.rs::icsr_in_process_forwards_count_against_the_federation_bounds`; `fed_bounds_5_7.rs::oversized_declared_response_is_skipped_with_warning` |
| C6 | T | a write forwarded into B (the matrix marks create, update, append, merge and delete as forwarded) | Decided (worker-2, owner may override): a grant is read-only unless it says read-write, because a write into another tenant is the larger harm and needs the explicit word; B's engine and the grant's view judge each forwarded write with the write rules of ADR-0023 rule 7 | `icsr_grant.rs::icsr_a_write_forward_needs_a_read_write_grant` |
| C7 | I | B's data keeps arriving in A after the grant is revoked (csource notifications, cached registrations) | the grant is re-read on every forward and every csource notification | `icsr_grant.rs::icsr_a_revoked_grant_stops_forwards`; `icsr_notify.rs::icsr_a_revoked_grant_stops_notifications` (proposed, T-3700) |
| C8 | E | an internal tenant (`snap-<uuid>`, the reserved tenant) as a target | the target must be a client-addressable tenant | `icsr_grant.rs::icsr_an_internal_tenant_is_not_a_target`; `tenant_isolation.rs::internal_tenants_are_not_addressable_by_a_client` |
| C9 | R | B cannot tell what A read | B's access log line and change events name the registration id and the source tenant | `icsr_gate.rs::icsr_a_forward_is_logged_in_the_target_with_the_registration_id` |
| C10 | I | B's error text reaches A | a failed in-process forward becomes the `NGSILD-Warning` of TS 104 176 6.3.5 (CIM 009 6.3.17, the warning an HTTP peer failure raises today), without B's `detail`; at most `MAX_PEER_WARNINGS` | `icsr_gate.rs::icsr_a_target_error_reaches_the_source_without_its_detail`; `fed_bounds_5_7.rs::peer_warnings_reach_the_client` |

## 9. module-rules

This section is written to be adopted verbatim by T-3443. "Red" means the named CI step exits non-zero, which fails the job `Workspace tests` of `.github/workflows/workspace.yml`; that workflow is called by `ci.yml` (pull requests into `dev`, `ci.yml:31-34`) and by `full.yml`, so every rule gates both pipelines.

### 9.1 Prerequisite: the operation layer `antares-ops`

Today no operation-layer crate exists. `antares-api` depends on `antares-store` and `antares-sql` (normal edges, Q1 below), and 17 of its non-test source files call store or SQL symbols directly (Q2: `lib.rs` 14 distinct targets, `snapshots.rs` 12, `notify.rs` 11, `temporal.rs` 9, `entities.rs` 9, `subscriptions.rs` 8, `entity_map.rs` 7, `state.rs` 6, `contexts.rs` 5, `batch.rs` 5, `csource.rs` 5, `distsub.rs` 4, `history.rs` 3, `types_attrs.rs` 2, `attrs.rs` 2, `federation.rs` 2, `repr.rs` 1). The handlers are the operations, and the seam (`gate!`, `crates/antares-api/src/lib.rs:27`; `policy`, ADR-0020) sits inside them. A surface crate today could only take `AppState`, which hands it the store (F1).

Decided (worker-2, owner may override): a new crate **`antares-ops`** is the operation layer every surface depends on, built in round 1 over `AppState::call`, because that call already runs "negotiation, the bounds wall, tenancy, the policy seam, history and notifications ... exactly once and exactly as they do for an NGSI-LD client" (`state.rs` doc comment above `call`, `:535-570`) and needs no handler to move; options: `antares-ops` over `AppState::call` now and the store calls moved behind it later (chosen), extract the 17 files' operations into `antares-ops` first (weeks of moves before any surface), let surfaces depend on `antares-api` (gives them `AppState::store`, F1).

`antares-ops` provides:

- `Ops`: a handle with private fields only (the `AppState` inside is never reachable), cloned into each surface router;
- `Ops::run(op: OperationId, request) -> response`, where `OperationId` is the closed set of `operation-matrix.yaml` row ids and the request type has no field for `NGSILD-Tenant`, `NGSILD-Snapshot` or a subject header (G2): those come from the outer request only;
- the view table (ADR-0023: `View`, its loader, its revision, resolution by id), because every non-NGSI-LD surface serves through exactly one view; and the composition hook that hands the view's predicate to `gate!` (compiled in `antares-api` only under its feature `views`, which `antares-ops` enables);
- `antares_ops::Surface`, the trait a surface implements (`router(&self, ops: Ops) -> Router`), adapted to `antares_api::ApiSurface` inside `antares-ops`;
- the error mapping of section 10.3 and the policy seam's public types re-exported as `antares_ops::policy` (`Decision`, `Filter`, `Subject`), so a surface never names `antares-api`.

Allowed edges (`[dependencies]`, normal and build):

| crate | role | may depend on (workspace crates) | must not depend on |
|---|---|---|---|
| `antares-ops` | `ops` | `antares-api` (features `views`), `antares-model`, `antares-ql` | `antares-store`, `antares-sql`, `antares-bus`, `antares-notifier`, any `surface` or `protocol` crate |
| `antares-views` | `surface`, feature `views` | `antares-ops` | everything else in the workspace |
| `antares-odata` | `surface`, feature `odata` | `antares-ops`, `odata-url`, `odata-csdl`, `odata-json`, `odata-batch`, `odata-axum`, `linkml-model` (T-3441 section 9: the CSDL model is built from the LinkML model) | everything else |
| `antares-mcp` | `surface`, feature `mcp` | `antares-ops` (protocol library `rmcp` is a registry crate) | everything else |
| `antares-linkml` | `surface`, feature `linkml` | `antares-ops`, `linkml-model` | everything else |
| `antares-ws` | `surface`, feature `ws` (ADR-0003) | `antares-ops` | everything else |
| `antares-icsr` | `surface`, feature `internal-csr` | `antares-ops` | everything else |
| `odata-url`, `odata-csdl`, `odata-json`, `odata-batch`, `odata-axum` | `protocol` | each other only as ADR-0024 decision 1 draws it (`odata-url` → `odata-csdl` behind its feature `edm`; `odata-json` → `odata-csdl`, `odata-url`; `odata-axum` → the four, behind feature `axum`) | any `antares-*` crate, in every dependency kind |
| `linkml-model` | `protocol` | none | any `antares-*` crate, in every dependency kind |

`dev-dependencies` of a surface may add `antares-ops` with its feature `test-kit` (a broker over the built-in store for integration tests) and nothing else from the workspace. `antares-broker` gains one optional dependency per surface crate, one feature per surface (the names in the table), and `full = ["views", "odata", "mcp", "linkml", "ws", "internal-csr"]`; `default` stays `["mqtt"]` (`crates/antares-broker/Cargo.toml:56`).

Every workspace package declares its role, so a crate without one is a red build rather than an unchecked one:

```toml
[package.metadata.antares]
role = "surface"            # core | ops | surface | protocol | addon
feature = "odata"           # surface only: the antares-broker feature that enables it
protocols = ["odata-url", "odata-csdl", "odata-json", "odata-batch", "odata-axum", "linkml-model"]   # surface only
```

The eleven existing crates are `core`, `examples/plugin-example` is `addon`.

### 9.2 Today's crate graph (cg)

```cypher
// Q1: workspace crate edges (42 rows at 7185e1a)
MATCH (a:Package)-[d:DEPENDS_ON]->(b:Package)
WHERE a.workspace AND b.workspace AND a.lang='rust'
RETURN a.name, b.name, d.kind ORDER BY a.name, b.name;
// Q2: antares-api source files that call store or SQL symbols (17 rows)
MATCH (f:File)-[:DEFINES]->(s:Symbol)-[c:CALLS]->(t:Symbol)
WHERE f.package='antares-api' AND NOT f.is_test AND NOT s.is_test
  AND t.package IN ['antares-store','antares-sql']
RETURN f.path, count(DISTINCT t.path) AS targets, count(c) AS edges ORDER BY targets DESC;
// Q3: unsafe in workspace code (0)
MATCH (s:Symbol) WHERE s.lang='rust' AND NOT s.external AND s.is_unsafe RETURN count(s);
```

Q1 normal edges at `7185e1a`: `antares-api` → `antares-jsonld`, `antares-matcher`, `antares-model`, `antares-notifier`, `antares-ql`, `antares-sql`, `antares-store`; `antares-broker` → `antares-api`, `antares-bus`, `antares-jsonld`, `antares-model`, `antares-plugin-example` (optional, feature `plugin-example`), `antares-sql`, `antares-store`; `antares-bus` → `antares-model`; `antares-jsonld` → `antares-model`; `antares-matcher` → `antares-jsonld`, `antares-model`, `antares-ql`; `antares-notifier` → `antares-jsonld`, `antares-model`; `antares-plugin-example` → `antares-api`, `antares-model`, `antares-notifier`, `antares-ql`, `antares-store`; `antares-ql` → `antares-jsonld`, `antares-model`; `antares-sql` → `antares-model`, `antares-ql`, `antares-store`; `antares-store` → `antares-model`, `antares-ql`; `antares-wasm` → `antares-api`, `antares-jsonld`, `antares-sql`. The other rows are `dev` edges.

### 9.3 Rule M1: each surface is its own crate behind its own cargo feature; a protocol library has no dependency on any `antares-*` crate

- **Enforced by:** new script `dev/check-module-rules.py`, step "Module rules" in `.github/workflows/workspace.yml`, placed right after the step "No addon in the shipped build" (`workspace.yml:258`).
- **Input:** `cargo metadata --format-version 1 --locked --no-deps`. It reads `packages[]` (`name`, `manifest_path`, `metadata.antares`, `features`) and each package's `dependencies[]` (`name`, `kind` = `null` | `"dev"` | `"build"`, `optional`, `uses_default_features`, `path`); `name` is the real package name, so a `package = "..."` rename cannot hide an edge.
- **Red when:** a workspace package has no `metadata.antares.role`; a `protocol` package has any dependency, of any kind, whose name starts with `antares-`; a `surface` package is not an `optional = true` dependency of `antares-broker`; its `metadata.antares.feature` is not a key of `antares-broker`'s `features`, or that feature does not contain `dep:<crate>`, or two surfaces name the same feature; the transitive closure of `antares-broker`'s `default` feature contains a surface feature or `dep:` of a surface, `ops` or `protocol` crate.
- **Self-test:** `dev/check-module-rules.py --self-test` runs every check against a synthetic metadata document with exactly one violation per red condition and fails unless each is reported (the same discipline as `dev/check-no-addon.sh:32-37`); the step runs `--self-test` first, then the real check.
- **Each feature builds and tests alone:** the feature matrix of T-3465 runs `cargo test -p antares-broker --no-default-features --features <f>` per surface feature; that is T-3465's job, M1 only guarantees the features exist one-to-one.

### 9.4 Rule M2: a surface crate depends only on the operation layer, the policy seam (through `antares-ops`) and its protocol library; never on `antares-store` or SQL

- **Enforced by:** the same `dev/check-module-rules.py`, same step, plus a symbol-level query for reviewers and for the T-3446 gate.
- **Input:** as M1, plus a read of `crates/antares-ops/src/**/*.rs`.
- **Red when:** a `surface` package's normal or build dependencies inside the workspace are not a subset of `{antares-ops} ∪ metadata.antares.protocols`, or a listed protocol is not a `protocol` package; its dev-dependencies inside the workspace are anything other than `antares-ops`; the `ops` package has a normal or build dependency on `antares-store`, `antares-sql`, `antares-bus`, `antares-notifier` or any `surface`/`protocol` crate; a file under `crates/antares-ops/src/` matches `pub use antares_(store|sql)`, `pub use antares_api::(AppState|state)`, or declares a `pub` field or a `pub fn` returning `AppState` (the re-export that would hand F1 back).
- **Self-test:** as M1, one synthetic violation per condition, and a scratch file under a temporary copy of `crates/antares-ops/src/` for the grep half.
- **Symbol-level check** (run with `cg build --reuse` after any move; the GOAL "cg query" form of the rule):

```cypher
// M2: no surface symbol touches a store/SQL symbol or AppState's store fields (expected: 0 rows)
MATCH (s:Symbol)-[r:CALLS|REFERENCES]->(t:Symbol)
WHERE s.package IN ['antares-views','antares-odata','antares-mcp','antares-linkml','antares-ws','antares-icsr']
  AND (t.package IN ['antares-store','antares-sql']
       OR t.path IN ['antares_api::state::AppState::store','antares_api::state::AppState::temporal'])
RETURN s.package, s.path, t.path, r.line;
// M1: no protocol crate depends on an antares-* crate (expected: 0 rows)
MATCH (a:Package)-[d:DEPENDS_ON]->(b:Package)
WHERE (a.name STARTS WITH 'odata-' OR a.name = 'linkml-model') AND b.name STARTS WITH 'antares-'
RETURN a.name, b.name, d.kind;
```

Whether `cg` can be installed on a GitHub runner: unknown. Until it can, the two queries run in the T-3446 gate and in review, and `dev/check-module-rules.py` is the CI enforcement.

### 9.5 Rule M3: feature off = no route registered and no code linked

- **Enforced by:**
  1. `dev/check-no-addon.sh`, `FORBIDDEN` (`dev/check-no-addon.sh:22`) extended with `antares-ops|antares-views|antares-mcp|antares-linkml|antares-ws|antares-icsr|odata-|linkml-model|rmcp|tungstenite` (`antares-odata` is already in it). It runs in `workspace.yml:258`, `full.yml:365` and `build-image.yml:52`; its self-test (`:34`) is changed to build the tree with `--features full` as well as `plugin-example`, so the new names must match a full tree.
  2. New script `dev/check-vanilla-symbols.sh`, step "Vanilla binary links no surface" in `workspace.yml`, after the unit-test steps (it reuses their build cache). It runs `cargo build --locked -p antares-broker` (the `dev` profile, which is not stripped; release is, F6), then `nm --defined-only --demangle target/debug/antares` and greps for the surface and protocol crate paths (`antares_ops::`, `antares_views::`, `antares_odata::`, `antares_mcp::`, `antares_linkml::`, `antares_ws::`, `antares_icsr::`, `odata_url::`, `odata_csdl::`, `odata_json::`, `odata_batch::`, `odata_axum::`, `linkml_model::`, `rmcp::`, `tungstenite::`). Self-test: the same grep must match a `cargo build --locked -p antares-broker --features full` binary. It then starts the vanilla binary with `ANTARES_API_SURFACES=admin,odata` and requires a non-zero exit naming "unknown surface" (`crates/antares-broker/src/main.rs:478-486`).
  3. The same script in mode `--audit-info <binary>` in `build-image.yml` on the image's `/antares`: the Dockerfile builds with `cargo auditable` (`Dockerfile:37`), which embeds the package list, and `rust-audit-info` 0.5.4 (MIT OR Apache-2.0) prints it; no surface, ops or protocol crate may appear. This checks the shipped artefact itself, which is stripped.
  4. The route half: test `crates/antares-broker/src/main.rs::api_surface_tests::the_vanilla_shelf_is_admin_alone` (proposed, `#[cfg(not(any(feature = ...)))]` over the surface features), beside the existing `api_surface_tests::an_unknown_surface_is_fatal_and_lists_the_shelf` (`main.rs:1613`): `SURFACE_SHELF` (`main.rs:455-461`) is exactly `["admin"]`, and `GET` on one path of each surface prefix answers 404 through `router()`.
- **Red when:** a forbidden name is in the default `cargo tree`; a forbidden symbol is in the vanilla debug binary; the vanilla binary accepts a surface name; a forbidden package is in the image's embedded list; the shelf test fails; or any self-test fails to match the full build.

### 9.6 Rule M4: feature on but no view or policy configured = the surface does not start; it never serves allow-all

- **Mechanism:** a selected surface opens only with a valid configuration: for `views`, `odata`, `mcp`, `linkml` and `ws` at least one view that loaded (ADR-0023 rule 10), for `internal-csr` at least one export grant (C1). Without one the surface is **closed**: no route under its prefix is served (404, the answer for any unknown path), `/q/health` lists it as `{"state": "closed", "reason": "..."}`, and it opens when the view table reload (ADR-0023 rule 10) brings a valid view. Decided (worker-2, owner may override): closed-and-reported rather than a process exit, because views are written through `/q` of the running broker, so a fresh deployment that exited would have no way to receive its first view; options: closed and reported (chosen), refuse to start the process, serve the surface with an empty view. No code path builds an allow-all view: `View` has no `Default`, its selector needs at least one type, and no environment variable produces a view. A surface named in `ANTARES_API_SURFACES` that this binary was not built with stays fatal (`main.rs:482`).
- **Enforced by:** test file `crates/antares-broker/tests/surfaces_fail_closed.rs` (proposed), run by the step `dev/unit-junit.sh all-features --workspace --all-features` of `workspace.yml`, with tests `a_selected_surface_without_a_view_is_closed` (every surface selected, empty view table: every route of every surface router answers 404, health says closed), `a_surface_opens_when_its_first_view_loads` and `closes_again_when_its_last_view_is_removed`, `a_view_that_failed_validation_does_not_open_a_surface`, and `there_is_no_allow_all_view` (a `compile_fail` doc test that `View::default()` does not exist, plus a check that an empty selector is refused at load).
- **Red when:** any of those tests fails.

### 9.7 What the rules do not cover

- That the handlers behind `AppState::call` themselves stay free of a second data path: that is ADR-0020's route walk (`policy_gate_seam.rs::every_route_asks_the_policy_engine_exactly_once`) extended by G3.
- Moving the store calls of the 17 files behind `antares-ops` (a later round; rule M2 does not change when it happens, because surfaces never had the edge).

## 10. Code rules for every new crate

Applies to `antares-ops`, the six surface crates and the six protocol crates of section 9.1.

### 10.1 The `[lints]` table each new crate carries

Cargo's `[lints] workspace = true` cannot be combined with crate-level entries, and the workspace table sets only `unsafe_code = "forbid"`, `unexpected_cfgs`, `unwrap_used = "warn"` and an allow (`Cargo.toml:155-166`, F9). So each new crate carries its own complete table instead of inheriting, and the workspace table is left as it is for the existing crates (raising it is T-3468's):

```toml
[lints.rust]
unsafe_code = "forbid"
missing_docs = "deny"
unexpected_cfgs = { level = "warn", check-cfg = ["cfg(tokio_unstable)"] }

[lints.clippy]
pedantic = { level = "deny", priority = -1 }
unwrap_used = "deny"
expect_used = "deny"
panic = "deny"
indexing_slicing = "deny"
arithmetic_side_effects = "deny"
todo = "deny"
unimplemented = "deny"
dbg_macro = "deny"
print_stdout = "deny"
print_stderr = "deny"

[lints.rustdoc]
broken_intra_doc_links = "deny"
```

Plus, per crate:

- `#![forbid(unsafe_code)]` at the top of `src/lib.rs` as well (GOAL point 8 names the attribute; the lint table alone is invisible to a reader of the source and to a crate copied out of the workspace).
- A crate-local `clippy.toml` with `allow-unwrap-in-tests = true`, `allow-expect-in-tests = true`, `allow-panic-in-tests = true`, `allow-indexing-slicing-in-tests = true`, so `--all-targets` holds the request-path rules without forcing them on `#[test]` bodies. All four keys exist in the clippy shipped with rustc 1.99.0 (`strings` of the local `clippy-driver`; the repository pins `channel = "stable"` in `rust-toolchain.toml`). No such key exists for `arithmetic_side_effects` (none in that list), so test code in these crates uses `checked_*`/`saturating_*` as well, or a test module writes `#[expect(clippy::arithmetic_side_effects, reason = "...")]`; Decided (worker-2, owner may override): checked arithmetic in tests too, because an `expect`/`allow` is what the GOAL rule "no `allow(...)`" forbids; options: checked arithmetic (chosen), `#[expect]` with a reason.
- `overflow-checks = true` for the new crates in release: `[profile.release.package.<crate>] overflow-checks = true` in the root `Cargo.toml`, one block per new crate (GOAL point 8 names this or checked arithmetic; with `arithmetic_side_effects` denied both hold).
- No `allow(...)` anywhere in these crates. `needless_borrows_for_generic_args` is allowed workspace-wide because of a clippy memory blow-up on the `antares-api` test target (`Cargo.toml:164-166`); the new crates do not inherit that allow, so the lint keeps its default level there (warn, red under `-D warnings`).

CI: the clippy step becomes `cargo clippy --locked --workspace --all-targets --all-features -- -D warnings` (`workspace.yml:150` today has no `--all-features`, F7); `RUSTDOCFLAGS="-D warnings" cargo doc --locked --workspace --no-deps --document-private-items` already runs daily in `strict.yml` (job `hygiene`) and moves to `workspace.yml` for the new crates (`-p` each); `cargo +nightly miri test` (`strict.yml:240`, today `-p antares-model -p antares-ql -p antares-matcher`) adds `-p odata-url -p odata-csdl -p odata-json -p odata-batch -p linkml-model` (not `odata-axum`, which is an HTTP adapter) (pure parsers and emitters, GOAL point 8). Public items carry a doc example that compiles (`cargo test --doc`, already in `workspace.yml`).

### 10.2 Parser limits

Every parser takes its limits as an argument, with no hidden default on the request path (odata-library.md section 3: "Every limit is a field of a `Limits` struct passed by the caller"); counters use checked arithmetic and return a `LimitExceeded` error, never wrap or panic; size is checked before parse (the bounds wall's own rule, `bounds.rs:2-8`). Each parser has a fuzz target in `fuzz/fuzz_targets/` (today: `expand_entity`, `geo_params`, `parse_q`, `scope_q`); proposed: `odata_url_parse`, `odata_batch_multipart`, `odata_batch_json`, `odata_json_body`, `odata_csdl_read` (if CSDL reading is kept, odata-library.md 3.2), `mcp_cursor`, `mcp_tool_arguments` (MCP rule 14), `ws_client_message`, `view_config_load`.

### 10.3 Errors per surface, without leaking hidden names

One mapping, implemented once in `antares-ops` and rendered per surface. A message is either a fixed string per condition or, on the NGSI-LD view only, an echo of the caller's own input (bounded to 256 bytes, control characters escaped); the OData error body echoes no request value at all and has no `innererror` (ADR-0024 decision 5, T-3441 S3), and an MCP refusal's text is status and `title` only (MCP rule 1). It never contains a stored value, another Entity's id, a type or Attribute name the caller did not send, the view's predicate, a tenant name the caller did not send (V18), or the text of an inner 5xx (replaced by a fixed string and a request id; map S23). An engine's `Deny(String)` reason reaches the client as ADR-0020 states ("Fixed strings: they reach the client in a ProblemDetails `detail`"); a view's own refusals use fixed strings. "Hidden" and "absent" produce identical bytes wherever the table says "as absent".

| condition | NGSI-LD view (ProblemDetails, TS 104 176 6.3.2 / 6.3.3) | OData (JSON error object, matrix 7.1 cites JSON Format 21.1) | MCP (JSON-RPC 2.0) | WebSocket | LinkML |
|---|---|---|---|---|---|
| out-of-view or absent Entity | 404 `ResourceNotFound` (ADR-0023 rule 3) | 404, `code` `NotFound` | tool result `isError: true` with status and `title`; resource: `-32602` as an unknown resource (MC10) | error message, ProblemDetails `type` only | n/a |
| hidden or unknown name in the request | as absent: `q` selects nothing (rule 2) | 400, `code` `UnknownProperty`, same bytes for hidden and never-existing (O1) | `-32602`, same bytes (MC1) | error message `BadRequestData` | n/a |
| operation not allowed by the view | 405 + `Allow`, or 422 `OperationNotSupported` (rule 4) | 405 + `Allow`, or 501 for matrix 7.1 features | tool absent from `tools/list`; a call answers as unknown tool | error message `OperationNotSupported` | 405 + `Allow` |
| engine `Deny`, write leaving the view | 403 `urn:antares:error:AccessDenied` (ADR-0020) | 403, `code` `Forbidden` | `isError: true` with status 403 and `title` | error message `AccessDenied` | 403 |
| malformed input | 400 `BadRequestData` / `InvalidRequest` | 400, `code` `BadRequest`, `target` = the option name | `-32600` (envelope) or `-32602` (arguments) | close 1007 or 1008 | 400 |
| a limit exceeded | 413 / 414 bare, 400, 403 `TooComplexQuery` / `TooManyResults` (bounds table, section 2.3) | the same status, `code` `LimitExceeded`, `message` names the limit, never the value that was hidden | `isError: true` naming the argument to narrow (MCP section 8) | close 1009 | 403 `TooManyResults` |
| surface closed (M4) | 404 | 404 | 404 (HTTP) | upgrade 404 | 404 |
| a remote `@context` the request names cannot be retrieved (`Refusal::ContextNotAvailable`, T-3947) | 504 `LdContextNotAvailable` (TS 104 176 6.3.2, as the root), fixed detail: never the URL or the fetch error | 504, `code` `InternalError` (`read.rs` `refused`) | tool result `isError: true` with status 504 and `title` | error message with status 504 and the same ProblemDetails | 504 `LdContextNotAvailable` |
| inner failure | 500 `InternalError`, fixed detail and request id | 500, `code` `InternalError` | `-32603`, fixed message | close 1011 | 500 |

The error codes in the OData column are proposals (the matrix: "OData leaves the code service-defined"). Tests: G9, O20, MC18, LK8 and `views_errors.rs::views_errors_carry_no_hidden_name_and_no_inner_detail` (proposed).

## 11. Supply chain

### 11.1 Gates

| gate | today | change | CI step | red when |
|---|---|---|---|---|
| `cargo deny` 0.20.2 | `check licenses advisories` per run of `workspace.yml` (`:209-215`); `check all` weekly in `advisories.yml` (F5) | per run: `cargo deny --log-level warn check licenses advisories bans sources`; `deny.toml` `[bans]` gains `deny = [{ name = "openssl-sys" }, { name = "native-tls" }, { name = "aws-lc-sys" }]`, the crates the workspace comments already keep out ("aws-lc-sys (C + cmake) never enters the build", `Cargo.toml:70-73`; one rustls stack on ring, `Cargo.toml:49-54`) | `workspace.yml` step "cargo-deny (licenses + advisories)" renamed "cargo-deny" | a licence off the allow list (`deny.toml:4-17`), an advisory not in `deny.toml:21-40`, a yanked crate, a wildcard version, a banned crate, an unknown registry or git source |
| `cargo audit` 0.22.2 (Apache-2.0 OR MIT, crates.io 2026-06-05) | not run; the Dockerfile notes "`cargo audit bin /antares` can then verify a shipped broker" (`Dockerfile:16-19`) | `cargo audit --deny warnings` on `Cargo.lock`, and `cargo audit bin` on the vanilla and the full image binaries; the ignore list lives in `deny.toml` alone and is passed as `--ignore <id>` flags generated from it by a new `dev/advisory-ignores.py`, so the two tools cannot drift | `workspace.yml` new step "cargo-audit"; `build-image.yml` new step "cargo audit bin" | an advisory or warning (unmaintained, yanked) not on the `deny.toml` ignore list |
| `cargo machete` 0.9.2 (MIT) | daily in `strict.yml` job `hygiene` (`:41-44`), weekly in `xray.yml` | also per run in `workspace.yml` (no build needed) | `workspace.yml` new step "cargo-machete" | an unused dependency in any crate |
| CycloneDX SBOM, `cargo-cyclonedx` 0.5.9 (Apache-2.0, crates.io 2026-03-19) | none; an SPDX SBOM of the released image by syft on `v*` tags (`full.yml:417-421`) | two SBOMs per build of the broker: vanilla (default features) and full (`--features full`), JSON, from `crates/antares-broker/Cargo.toml`; uploaded as artefacts, attached to the GitHub release beside the syft SPDX file; the vanilla SBOM is grepped for the M3 forbidden names | `workspace.yml` new step "SBOM (vanilla and full)"; `full.yml` job `github-release` attaches them | `cargo cyclonedx` fails, either file is missing or empty, or the vanilla SBOM names a surface, ops or protocol crate |
| `default-features = false` | not checked | `dev/check-module-rules.py` (section 9) also checks that every registry dependency of an `ops`, `surface` or `protocol` crate has `uses_default_features == false`, unless it is listed in `[package.metadata.antares] default-features-ok = [...]` with a reason | `workspace.yml` step "Module rules" | a default-on registry dependency without a listed reason |
| tool pins | `cargo-deny` pinned (`V=0.20.2`); `cargo install cargo-chef cargo-auditable --locked` without versions (`Dockerfile:20`) | pin `cargo-auditable@0.7.7` (crates.io 2026-10-02) and `cargo-chef`, and every new tool above, by exact version | the steps above, `Dockerfile` | n/a (a pin, reviewed) |

The exact command-line flags of `cargo-cyclonedx` 0.5.9 for selecting features and the output file name were not verified (the tool was not run): unknown, to be read from its `--help` when the step is written.

### 11.2 Every new dependency the surfaces need

Allow list: `deny.toml:4-17` (MIT, Apache-2.0, Apache-2.0 WITH LLVM-exception, BSD-2-Clause, BSD-3-Clause, EUPL-1.2, ISC, Unicode-3.0, Zlib, CDLA-Permissive-2.0). Versions and licences are the latest stable on crates.io read on 2026-10-10 unless the row says otherwise; "`unsafe` lines" is a grep of the crate tarball's `src/` for the word `unsafe` (comment lines not removed for the two crates grepped in this task, so the counts are upper bounds; the OData rows are copied from odata-library.md section 6).

| crate | version | licence | allowed | in `Cargo.lock` | `unsafe` | used by | features (`default-features = false`) | reason |
|---|---|---|---|---|---|---|---|---|
| `rmcp` | 3.5.1 (2026-10-05) | Apache-2.0 | yes | no | 0 lines in `rmcp-3.5.1/src`; no `forbid(unsafe_code)` attribute | `antares-mcp` | `server`, `transport-streamable-http-server` (MCP 6.3); its `default` is `base64`, `macros`, `server` | the MCP protocol library; exact pin `=3.5.1` (MCP 6.3: a minor release already changed which revision `LATEST` names) |
| `rmcp-macros` | 3.5.1 | Apache-2.0 | yes | no | not grepped | only if `macros` is enabled | not enabled by the chosen feature set | listed because `rmcp`'s default pulls it |
| `schemars` | 1.2.2 (2026-07-27) | MIT | yes | no | not grepped | `rmcp` feature `server` | as `rmcp` sets | JSON Schema of tool inputs |
| `pastey` | 0.2.3 | MIT OR Apache-2.0 | yes | no | not grepped | `rmcp` feature `server` | n/a (proc-macro) | pulled by `rmcp` |
| `sse-stream` | `^0.2.4` required by `rmcp` (0.2.4 to 0.2.6 and 0.3.0 all MIT OR Apache-2.0) | MIT OR Apache-2.0 | yes | no | not grepped | `rmcp` feature `server-side-http` | as `rmcp` sets | streamed responses (`subscriptions/listen`) |
| `tokio-stream` | 0.1.19 | MIT | yes | yes, 0.1.19 (`Cargo.lock:3671`) | not grepped | `rmcp` | as `rmcp` sets | |
| `hmac` | 0.13.0 | MIT OR Apache-2.0 | yes | yes, 0.13.0 (`Cargo.lock:1372`) | not grepped | `antares-mcp` (`requestState`, MCP D-11) | default | HMAC-SHA256 of `requestState`; directly or through `rmcp`'s `request-state` feature: T-3442 decides |
| `sha2` | 0.11.0 | MIT OR Apache-2.0 | yes | yes, 0.10.9 and 0.11.0 (`Cargo.lock:3078`, `:3089`) | not grepped | as `hmac` | default | as `hmac` |
| `tokio-tungstenite` | 0.29.0 (latest is 0.30.0) | MIT | yes | no | 0 lines | `antares-ws` directly, `default-features = false`, feature `handshake` (T-3704) | `handshake` only | WebSocket upgrade and framing |
| `tungstenite` | 0.29.0 | MIT OR Apache-2.0 | yes | no | 5 lines: `src/utf8.rs:59`, `:99` and `src/protocol/frame/utf8.rs:20`, `:28` (`from_utf8_unchecked`), `src/protocol/frame/mask.rs:26` (`align_to_mut`) | as above | as axum sets | the unavoidable `unsafe` GOAL point 8 asks to name; limits set explicitly (F8, W3, W4) |
| `sha1` | `^0.10` required by tungstenite's `handshake` feature (0.11.0 in the lock at `Cargo.lock:3061`; 0.10.x licence not read) | 0.11.0: MIT OR Apache-2.0 | yes for 0.11.0; 0.10.x unknown | 0.11.0 | not grepped | tungstenite `handshake` (`derive_accept_key`, the `Sec-WebSocket-Accept` hash) | as tungstenite sets | |
| `quick-xml` | 0.42.0 (2026-08-22) | MIT | yes | no | `#![forbid(unsafe_code)]` (odata-library.md 6) | `odata-csdl` | `serialize` only if reading CSDL is kept | CSDL XML writer for `$metadata` |
| `memchr` | 2.8.3 | Unlicense OR MIT | yes (MIT) | yes (`Cargo.lock:1925`) | yes, SIMD (odata-library.md 6) | `odata-batch`, `quick-xml` | `std` | multipart boundary search |
| `serde`, `serde_json` | 1.0.229, 1.0.151 | MIT OR Apache-2.0 | yes | yes | odata-library.md 6 | every protocol crate | `derive` / `std` | |
| `percent-encoding` | 2.3.2 | MIT OR Apache-2.0 | yes | yes (`Cargo.lock:2239`) | 4 lines (odata-library.md 6) | `odata-url` | default | URL decoding |
| `thiserror` | 2.0.21 | MIT OR Apache-2.0 | yes | yes (`Cargo.lock:3516`) | not counted | every new crate | default | error types |
| `indexmap` | 2.14.2 (lock 2.14.0, `Cargo.lock:1736`) | Apache-2.0 OR MIT | yes | yes | not counted | `odata-json`, `linkml-model` | `std` | member order in written documents |
| `axum`, `http` | 0.8.9, 1.5.0 (odata-library.md 6) | MIT; MIT OR Apache-2.0 | yes | yes (`Cargo.lock:378`, `:1380` per odata-library.md 6) | not counted | `odata-axum` | axum without defaults, as `crates/antares-api/Cargo.toml:52` already does | the HTTP adapter of ADR-0024 decision 1 |
| `nom` | 8.0.0 | MIT | yes | yes | 4 lines (odata-library.md 6) | none in round 1 | | fallback parser only; the chosen parser is hand-written (odata-library.md 6) |
| `geo` | 0.33.1 | MIT OR Apache-2.0 | yes | yes, 0.33.1 (`antares-ql` already links it) | 2 matches of `unsafe` in `geo-0.33.1/src` | `antares-residual` (T-3756) | `default-features = false`, as the workspace sets | DE-9IM `relate` of the residual's spatial relations, the matrix `antares-ql` matches `georel` with |
| `serde-saphyr` | 1.3.0 (2026-09-16) | MIT OR Apache-2.0 | yes | no | forbids it (T-3441 section 9) | `linkml-model` dev-dependency only | default | YAML parse in the round-trip property test (LK3); no YAML parser at runtime (section 6, T-3441 section 9); `#![forbid(unsafe_code)]` per T-3441 section 9 |
| `proptest` | 1.11.0 | MIT OR Apache-2.0 | yes | no | not counted | dev-dependency of every new crate | default | property tests (GOAL point 5) |
| `arbitrary` | 1.5.0 | MIT OR Apache-2.0 | yes | no | not counted | `fuzz/` only | `derive` | fuzz input types; `fuzz/` is outside the root `cargo deny` (excluded, `Cargo.toml:21`), so its `libfuzzer-sys` (NCSA, not on the list) stays as today |

Tools, not dependencies: `cargo-deny` 0.20.2 (MIT OR Apache-2.0), `cargo-audit` 0.22.2, `cargo-machete` 0.9.2, `cargo-cyclonedx` 0.5.9, `cargo-auditable` 0.7.7 (MIT OR Apache-2.0), `rust-audit-info` 0.5.4 (MIT OR Apache-2.0, last release 2024-05-08).

## 12. Decisions, unknowns and counts

### 12.1 Decisions taken here (worker-2, owner may override)

| # | decision | section |
|---|---|---|
| D1 | `antares-ops` over `AppState::call` is the operation layer; surfaces depend on it alone | 9.1 |
| D2 | the view table and the gate composition hook live in `antares-ops` / `antares-api` feature `views`, so every surface serves through one view | 9.1 |
| D3 | M4 closes an unconfigured surface (404, reported in `/q/health`) instead of exiting the process | 9.6 |
| D4 | every new crate carries its own complete `[lints]` table; the workspace table is not raised here | 10.1 |
| D5 | checked arithmetic in test code too (no `#[expect]`) | 10.1 |
| D6 | a Replace through a view is refused when the stored Entity holds hidden Attributes | V11 |
| D7 | a Relationship target must be visible, and an absent target answers alike | V13 |
| D8 | `MAX_Q_NODES` counts the caller's `q` alone; the view predicate is bounded at load | V23 |
| D9 | reaching `MAX_JOIN_LOOKUPS` in an OData `$expand` fails with 400, never a partial answer | O6 |
| D15 | `Cache-Control: private, no-cache` on `$metadata` and the LinkML documents (amends T-3441 7.4) | O16, LK2 |
| D10 | a JSON-RPC batch array is refused with `-32600` | MC6 |
| D11 | (taken by T-3441 section 9, adopted here) the LinkML surface writes YAML with its own emitter and parses none | 6 |
| D12 | an oversized LinkML schema is refused when it is written | LK7 |
| D13 | internal CSR needs an export grant held by the target tenant, checked at registration and on every forward | C1 |
| D14 | an export grant is read-only unless it says read-write | C6 |

### 12.2 Unknown or not done

- `cg` on a GitHub runner: unknown; the M1/M2 Cypher runs in review and in T-3446 until it is (9.4).
- Whether MCP 2026-07-28 allows JSON-RPC batching: not re-read; MC6 refuses arrays either way.
- `cargo-cyclonedx` 0.5.9 feature and output flags: not verified (11.1).
- `sha1` 0.10.x licence: not read; `unsafe` not grepped for `rmcp-macros`, `schemars`, `pastey`, `sse-stream`, `tokio-stream`, `hmac`, `sha2`, `sha1`, `serde-saphyr`.
- No 414 rule found in TS 104 176 clause 6; the bounds wall's 414 rests on CIM 009 6.3.4 comments (2.3).
- `docs/analysis/mcp-surface.md` is a draft; MCP rule 2's `antares-api` edge conflicts with M2 (section 5 note).
- `remote_notify` and `pre_notify` (matrix section 8 item 13) were not traced here either; WebSocket and MCP delivery assume `deliver_as` (W5, MC17).
- Every test marked "(proposed)" does not exist yet. The STRIDE test cells name 168 distinct tests, fuzz targets and scripts: 30 exist (each checked to be a unique `fn` under `crates/`), 138 are proposed (counted by a script over the test column); proposed names taken from ADR-0023, T-3441 and T-3442 are marked with their source and are listed without a file where the source gave none.

### 12.3 Row counts

| table | rows |
|---|---|
| 2.3 bounds | 20 |
| 2.4 findings | 9 |
| 2.5 cross-surface STRIDE (G) | 10 |
| 3 NGSI-LD view STRIDE (V) | 27 |
| 4 OData STRIDE (O) | 26 |
| 5 MCP STRIDE (MC) | 20 |
| 6 LinkML STRIDE (LK) | 8 |
| 7 WebSocket STRIDE (W) | 11 |
| 8 internal CSR STRIDE (C) | 10 |
| STRIDE rows in all | 112, each with a mitigation and at least one test name |
| 9.1 allowed edges | 9 |
| module rules | 4 (M1 to M4), each with its CI step |
| 10.3 error mapping | 8 |
| 11.1 supply-chain gates | 6 |
| 11.2 new dependencies | 22 |
| 12.1 decisions | 15 |

## 13. Related

- `tasks/GOAL-antares-surfaces.md` points 5, 6 and 8; tasks T-3443 (architecture ADR, adopts section 9), T-3444 (test plan, takes every test name above), T-3445 (files the round), T-3446 (gate), T-3462 to T-3468 (baseline), T-3470 (join visibility).
- `docs/analysis/operation-matrix.md`, `docs/analysis/gateway-to-broker-map.md`, `docs/analysis/odata-library.md`, `docs/analysis/mcp-surface.md`, `docs/analysis/cim009-to-ts104175.md`
- `docs/adr/ADR-0003-websocket-deferred.md`, `ADR-0010-egress-allow-private-by-default.md`, `ADR-0011-tenant-specific-context-source-alias.md`, `ADR-0014-extension-hooks-model.md`, `ADR-0020-policy-seam.md`, `ADR-0023-endpoint-views.md`
- `crates/antares-api/src/bounds.rs`, `src/surface.rs`, `src/state.rs`, `src/policy.rs`, `src/lib.rs`; `crates/antares-broker/src/main.rs`; `dev/check-no-addon.sh`; `.github/workflows/workspace.yml`, `full.yml`, `strict.yml`, `advisories.yml`, `build-image.yml`, `xray.yml`; `deny.toml`; `Dockerfile`
