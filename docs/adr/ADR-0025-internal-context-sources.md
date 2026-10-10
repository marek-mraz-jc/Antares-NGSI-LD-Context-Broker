# ADR-0025 — Internal context sources: a registration that names this broker is served in process

Date: 2026-10-10. Status: accepted. Decided by worker-2 under the owner
order of 2026-10-10 ("a design decision is yours: pick the recommended
option"); the owner may override any rule below. Builds on ADR-0011 (the
tenant-qualified Via alias), ADR-0016 (bindings behind the sink registry)
and ADR-0020 (the policy seam); supersedes none. Task T-3439.

## Context

Cross-tenant federation inside one broker is a deployment shape Antares
already serves (ADR-0011): tenant A holds a Context Source Registration
whose `endpoint` is this broker and whose `tenant` member names tenant B.
Every such forward leaves the process today. `federation::forward`
(`crates/antares-api/src/federation.rs:858`) builds a `reqwest` request,
passes the egress guard, opens a socket to the broker's own address, and
the request comes back in through the listener, the router and tenant B's
handler. The joinedcontext platform wants one binary to serve Context
Spaces that read each other this way (`tasks/GOAL-antares-surfaces.md`,
point 2), and the socket hop costs a connection slot, a second JSON
serialisation, the egress check and the 8 s forward ceiling for data that
sits in the same store.

What the spec fixes and an in-process path must keep:

- TS 104 175 7.8.1: "When contacting the respective Context Sources, the
  Tenant information from the Context Source Registration has to be used.
  If no Tenant information is present in the Context Source Registration,
  no Tenant information is to be used and thus the default Tenant is
  targeted on the registered Context Source."
- TS 104 175 9.7: "a specific field listing all previously encountered
  Context Sources (e.g. a Via header ...) shall be passed as part of the
  request and this field can be used to exclude duplicated sources from
  matching as Context Source Registration. If the requested endpoint is
  also listed as previously encountered when a Context Source receives a
  call to that endpoint, this is an indication that a new loop has been
  detected. A suitable error shall be returned, indicating that the loop
  has been detected. In the case of multi-tenancy (see clause 7.8) each
  Tenant found within each registered Context Source shall be considered
  separately."
- TS 104 176 6.3.5: "508 Loop Detected - if the single registered source
  and tenant is registered to redirect back on to the Context Broker".

The code map (cg graph built at `4c3610cd5b`; rerun with `cg q -e`):

```cypher
// every caller of the one forward function: the transport choke point
MATCH (c:Symbol)-[:CALLS]->(f:Symbol {path:'antares_api::federation::forward'})
RETURN DISTINCT c.path;
// -> batch::forward_one, batch::forward_batch, batch::batch_delete,
//    federation::{fed_retrieve, fed_query, fed_retrieve_temporal,
//    fed_query_temporal, fed_entity_maps, forward_part}, distsub::forward_sub
// every reader of the registration matcher
MATCH (c:Symbol)-[:CALLS]->(f:Symbol {name:'matching_regs'}) WHERE NOT c.is_test
RETURN DISTINCT c.path, c.file;
// -> federation::{fed_query, fed_retrieve, fed_query_temporal,
//    fed_retrieve_temporal, write_regs, fed_entity_maps},
//    entity_map::build_retrieve_map, entities::retrieve_entity_inner
// the in-process router entry the façades already use
MATCH (c:Symbol)-[:CALLS]->(f:Symbol {path:'antares_api::state::AppState::call'})
RETURN DISTINCT c.path, c.file;
// -> examples/plugin-example surface::things, tests/facade_seam_f0.rs
```

So every distributed operation reaches the network through
`federation::forward` (ten callers), every candidate set comes from
`federation::matching_regs` (`federation.rs:524`, eight callers), the Via
pseudonym from `federation::alias_for` (`federation.rs:210`), the loop rule
from `federation::via_loop` / `handle_via_loop` (`federation.rs:265`,
`:276`), and the merge plus the ADR-0020 post-merge filter run in the
callers on the `(status, body, warnings)` triple `forward` returns. And the
broker already owns an in-process entry into its own router:
`AppState::call` (`crates/antares-api/src/state.rs:535`), with a per-task
depth bound (`bounds::MAX_IN_PROCESS_CALL_DEPTH = 8`,
`crates/antares-api/src/bounds.rs:107`).

## Decision

An internal context source is **a transport inside `federation::forward`,
nothing else**. Matching, mode handling, operation lists, Via, loop
detection, merge and policy stay on the one code path they are on today;
only the step that turns a built request into `(status, body, warnings)`
changes.

### 1. "This broker" is configured, never guessed

`ANTARES_SELF_URLS`: a comma-separated list of absolute `http`/`https`
base URLs under which this deployment is reachable (the in-cluster service
URL, the public ingress URL with its path prefix). Validated at start:
absolute, no userinfo, no query, no fragment; scheme and host lowercased,
the default port made explicit, a trailing `/` removed. An invalid entry
stops the broker (a misread self URL would send tenant data to the wrong
place, so it is not a warning).

A registration endpoint is internal when, normalised the same way, it
equals a self URL or starts with a self URL followed by `/`. An endpoint
carrying userinfo is never internal (it is going somewhere that wants
credentials). No DNS lookup, no comparison with the pod IP, no use of
`ANTARES_PUBLIC_URL` or `host_alias` as an implicit self URL: the list is
empty by default, and an empty list means every forward is HTTP.

Decided (worker-2, owner may override): explicit list, empty by default,
because a derived self URL (`http://{host_alias}`) names some other
broker in a deployment that reuses the alias, and a DNS answer is input
an attacker who controls a resolver writes. Options: explicit list;
`ANTARES_PUBLIC_URL` as an implicit member; resolve the endpoint host and
compare with local addresses.

### 2. The request is the HTTP request, delivered without a socket

`forward` builds the request exactly as it does today (method, path,
query, `Via` from `outbound_via(headers, &alias_for(..))`, `Link` or
inline `@context`, `NGSILD-Tenant` from the registration only, the
4.3.6.5 `contextSourceInfo` headers minus the subject headers, the
4.3.6.6 content-type rules, the `local=true` of a `localOnly`
registration). Only the last step branches:

- **HTTP** (today): `st.fed_http` sends it, after `egress.check_url`, the
  breaker and the 5.2.34 cooldown.
- **Internal**: the URL's self-URL prefix is cut off (the router serves
  `/ngsi-ld/v1/...` without the ingress prefix) and the request goes to
  the router through `AppState::call`. The 5.2.34 cooldown and the
  per-registration timeout (`reg.timeout_ms`, capped at the same 8 s
  ceiling) apply unchanged; `bounds::FED_INFLIGHT` is still acquired, so
  the fan-out bound is the same. The egress guard and the host:port
  breaker are skipped, as ADR-0016 rule 5 skips them for a sink with
  `network() == false`: no socket is opened, and the breaker models a
  remote peer's latency, which an in-process call does not have.
- The response is read through the same cap as `read_body_capped`
  (`federation.rs:735`), its `NGSILD-Warning` values through the same
  `MAX_PEER_WARNINGS` cut, and the triple returned is the HTTP triple.
  A timeout is 504, as for HTTP; there is no 503 class (no connection to
  refuse).

`AppState::call` copies `NGSILD-Tenant`, `NGSILD-Snapshot` and `Link`
from its `caller` argument when the request lacks them
(`state.rs:604`). A forward must not inherit any of them: a registration
without a `tenant` targets the default tenant (7.8.1), and a snapshot
read is never forwarded. The internal transport therefore passes a
caller map that holds the subject headers only (rule 4); a unit test
holds that a registration without `tenant` lands in the default tenant
when the triggering request named tenant A.

### 3. Loop detection and Via are the HTTP ones, because they are the same code

The inner request carries the `Via` the HTTP request would carry, with
`alias_for(host_alias, A)` appended. Tenant B's handler reads it with
`via_tokens`, `matching_regs` drops every registration whose
`contextSourceAlias` is already in the chain (ADR-0011 rule 4), and
`handle_via_loop` answers 508 only in 6.3.5's case (a single
`exclusive`/`redirect` source back to the same source and tenant). The
alias is `{alias}~{tenant}` for a non-default tenant, so an A → B → A
chain is one hop per tenant pair and a genuine A → A cycle is a loop,
exactly as over HTTP. `MAX_VIA_HOPS = 32` (`federation.rs:249`) holds the
chain length.

In-process depth has its own ceiling, `MAX_IN_PROCESS_CALL_DEPTH = 8`,
held per task by `AppState::call`. A forward that would exceed it does
not fail: it goes over HTTP to the same self URL, so a chain longer than
eight in-process hops behaves as it does today. The depth bound protects
the stack; it does not change what a client sees.

### 4. Tenant B's policy decides, about the caller who asked

ADR-0020: "The subject never travels" — the subject headers are stripped
from every forwarded request. That rule is about leaving the process. An
internal forward does not leave it, so it carries the subject: the
caller map handed to `AppState::call` holds the
`ANTARES_POLICY_SUBJECT_HEADERS` values of the triggering request, and
`policy::subject_of` (`crates/antares-api/src/policy.rs:490`) in tenant
B's handler builds `Subject { tenant: B, headers }` from them. Tenant B's
engine therefore decides about the real caller, under tenant B, exactly
as it would for a direct request to tenant B.

This is the security property the task asks for: a registration in
tenant A pointing at tenant B (anyone allowed to register in A can write
one) reads nothing from B unless B's engine grants the caller. A refusal
in B is a 403 to the forward; the merge reports it as 6.3.5's code 299
("An error response (such as 403 - Forbidden) was received from the
registration endpoint") and serves tenant A's own data. Over HTTP the
same registration reaches B with no subject at all, which an engine that
fails closed refuses and `AllowAll` serves. Under `AllowAll` (vanilla,
every ETSI cell) both transports give the same answer, and tenancy is not
an access control there anyway: any client can send `NGSILD-Tenant: B`.

Decided (worker-2, owner may override): the subject travels in process,
because the target tenant's engine has to decide about someone, and the
caller is the only honest answer; deciding about nobody either refuses a
legitimate grant or serves an anonymous one. Options: carry the subject
in process; strip it as over HTTP; require a per-pair allowlist
(`A→B`) in configuration as well. The allowlist is not adopted: it
duplicates what B's engine already decides and moves a data-access rule
out of the policy seam into broker configuration.

Views (ADR-0023) compose without a new rule: a view pins tenant A, and
the merged answer passes the view's predicate and allowlist after the
merge, as every federated answer does (ADR-0020's post-merge filter).

### 5. Modes and operation lists: unchanged by construction

`inclusive`, `exclusive`, `redirect` and `auxiliary` (9.3.2, 9.3.3), the
`operations` list (`FedReg::supports`, 4.3.6.1), `localOnly`, the
`contextSourceInfo` keys, Entity Maps (9.6) and precedence (9.3.4) are
decided before `forward` is called and do not know the transport. No
branch on "internal" exists outside `forward`; a `cg` query in CI holds
that (Confirmation).

### 6. Distributed subscriptions and their notifications

`distsub::forward_sub` sends the 5.8.1.4 subscription copy through
`forward`, so the copy is created in tenant B in process. Its
notification endpoint is `ANTARES_PUBLIC_URL` plus the ADR-0019 receiver
path. When that URL is a self URL, the HTTP sink delivers the
notification through `AppState::call` as well, with the same
recognition function; the 5.8.6 retry and dead-letter policy
(ADR-0015) is unchanged, since a delivery attempt is still one call that
answers a status. Notification delivery never carries a subject (it is
broker-initiated, ADR-0020), in process or not.

### 7. Feature `internal-csr`

`antares-api` gains the cargo feature `internal-csr`, off by default and
in no default feature set. Off: `ANTARES_SELF_URLS` is not read (a set
value logs one warning at start naming the missing feature), and
`forward` compiles to today's code. On with an empty list: identical to
off at runtime. The transport is one module
(`antares-api::federation::internal`), the only place that calls
`AppState::call` from federation.

## Consequences

- One binary serves Context Spaces that federate with each other without
  a socket per hop; HTTP federation with other brokers is unchanged.
- A deployment must list its self URLs. Forgetting one costs only the
  socket: that forward goes over HTTP as it does today.
- Under a subject-aware engine an internal forward can return data an
  HTTP forward to the same broker would not (the caller holds a grant in
  B), and it can refuse what `AllowAll` behind an HTTP hop served. Both
  are the target tenant's policy deciding; the difference is documented,
  not hidden.
- The egress guard no longer sees self-forwards. A self URL is the
  deployment's own address, which ADR-0010 already allows as private.

## Equivalence test plan

Three layers, each a named test filed by T-3445:

1. **Same request.** `internal_csr_builds_the_http_request`
   (`crates/antares-api/src/federation/internal.rs` unit test): for one
   `FedReg` per mode, with and without `tenant`, `localOnly`,
   `jsonldContext`, `contentType`, `urn:ngsi-ld:request` copies, an inline
   `@context` and an inbound Via chain, the `http::Request` handed to
   `AppState::call` equals the `reqwest::Request` the HTTP branch builds:
   method, path after the self prefix, query, every header except `Host`,
   `Content-Length` and the subject headers, and body bytes.
2. **Same answer.** `crates/antares-api/tests/internal_csr_equivalence.rs`
   runs every scenario below twice against one broker: once with tenant B
   reached over a bound loopback listener (feature on, `ANTARES_SELF_URLS`
   empty, so HTTP) and once in process (the listener URL listed as a self
   URL). Status, body (after `@context` normalisation), `NGSILD-Warning`
   values and the state left in both tenants must be equal. The scenarios
   are the federated ETSI test purposes, rebuilt as tenant-to-tenant
   cases because the suite's own Context Source is a mock HTTP server
   (`ngsi-ld-test-suite/resources/variables.py`, `context_source_host`)
   that an in-process transport cannot replace:

   | ETSI TPs (`ngsi-ld-test-suite/TP/NGSI-LD/DistributedOperations/`) | operation |
   |---|---|
   | D001_01_exc, D001_01_inc, D001_01_red, D001_02_exc, D001_02_inc, D001_03_01_inc, D001_03_02_inc, D001_03_03_inc, D001_04_inc, 5614_01, 436_02, 436_06 | Create Entity |
   | D002_01_exc, D002_01_inc, D002_01_red, D002_02_01_inc, D002_02_02_inc, D002_02_exc, D002_03_inc | Delete Entity |
   | D003_01_exc, D003_01_inc, D003_01_red, D003_02_exc, D003_02_inc, D003_02_red | Append Entity Attributes |
   | D004_01_exc, D004_01_inc, D004_01_red | Update Entity Attributes |
   | D005_01_exc, D005_01_inc, D005_01_red | Partial Attribute Update |
   | D006_01_exc, D006_01_inc, D006_01_red, D006_02_exc, D006_02_inc, D006_02_red | Delete Entity Attribute |
   | D007_01_exc, D007_01_inc, D007_01_red | Replace Entity |
   | D008_01_exc, D008_01_inc, D008_01_red | Merge Entity |
   | D009_01_exc, D009_01_inc, D009_01_red | Replace Entity Attribute |
   | D010_01_aux, D010_01_exc, D010_01_inc, D010_01_red, D010_02_exc, D010_02_inc, D010_02_red, D010_03_inc, 436_01, 436_04, 436_05, 6317_01 | Retrieve Entity |
   | D011_01_01_inc .. D011_01_05_inc, D011_01_aux, D011_01_exc, D011_01_red, D011_02_exc, D011_02_inc, D011_02_red, D011_03_inc, D011_04_inc, D011_05_inc, 5724_01, 5724_02, 6317_02 | Query Entities |
   | D012_01_exc, D012_01_inc, D012_01_red | Create Batch |
   | D013_01_exc, D013_01_inc, D013_01_red, D013_02_exc, D013_02_inc, D013_02_red | Upsert Batch |
   | D014_01_exc, D014_01_inc, D014_01_red, D014_02_exc, D014_02_inc, D014_02_red | Update Batch |
   | D015_01_exc, D015_01_inc, D015_01_red (and the `DeleteBatchEntities/D015_01_inc` duplicate id) | Delete Batch |
   | D016_01_exc, D016_01_inc, D016_01_red | Merge Batch |
   | D017_01_exc, D017_01_inc, D017_01_red | Purge Entities |
   | D018_01, D018_02, D018_03 | Loop detection (D018_01 keeps its recorded suite defect, ADR-0011) |
   | 5814_01, 5814_02 | Distributed subscription and its notification (rule 6) |

   The TP ids come from `find ngsi-ld-test-suite/TP/NGSI-LD/DistributedOperations -name '*.robot'`
   at `4c3610cd5b`; a CI step diffs that list against the test's scenario
   table, so a TP added upstream fails the job until it has a case.
3. **Same suite result.** The ETSI DistributedOperations directory runs
   against the `full` binary with `ANTARES_SELF_URLS` set to the broker's
   own URL (the mock stays an HTTP source, since it is not a self URL);
   the per-TP verdicts must equal the vanilla run. This proves the feature
   leaves foreign HTTP federation alone.

## Security

| threat | rule | named test |
|---|---|---|
| a registration in A reads tenant B without a grant | 4 | `internal_csr_cross_tenant_read_without_grant_is_refused`: a test engine grants subject `alice` tenant A only; A registers B (inclusive, internal); `alice` queries A and gets A's Entities, no B Entity, and a 299 `NGSILD-Warning` |
| the forward inherits the caller's tenant or snapshot | 2 | `internal_csr_without_tenant_targets_the_default_tenant`, `internal_csr_never_forwards_a_snapshot` |
| a spoofed self URL (userinfo, other port, prefix trick `https://self.example.evil`) | 1 | `internal_csr_self_url_matching_is_exact` (equal or `self + "/"` only; userinfo never internal) |
| an A → B → A chain loops in memory | 3 | `internal_csr_cycle_terminates_like_http` (ADR-0011's `cross_tenant_cycle_terminates_with_the_data`, in process) |
| stack exhaustion by a deep chain | 3 | `internal_csr_falls_back_to_http_past_the_depth_bound` |
| the subject leaves the process | 4, 6 | `policy_subject_on_the_wire_p6.rs` keeps passing for HTTP forwards; `internal_csr_notification_carries_no_subject` |

## Confirmation

- The tests above, in `crates/antares-api/tests/internal_csr_*.rs` and
  the `federation::internal` unit module, under `--features internal-csr`.
- CI `cg` gate (T-3447 module rules): no symbol outside
  `antares_api::federation::internal` calls `AppState::call` from
  `antares_api::federation` or `antares_api::batch`:
  `MATCH (c:Symbol)-[:CALLS]->(f:Symbol {path:'antares_api::state::AppState::call'}) WHERE c.path STARTS WITH 'antares_api::' AND NOT c.path STARTS WITH 'antares_api::federation::internal' AND NOT c.is_test RETURN c.path`
  must return no row.
- The default build links no `federation::internal` symbol (the vanilla
  `cargo tree -e features` / symbol gate of the surfaces architecture
  ADR), and the ETSI DistributedOperations verdicts of the default build
  are unchanged.
