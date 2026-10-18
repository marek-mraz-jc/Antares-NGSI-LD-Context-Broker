// SPDX-License-Identifier: EUPL-1.2
//! Discovery: /types and /attributes (5.7.5–5.7.10; resources 6.25–6.28).

use crate::negotiate::*;
use crate::state::AppState;
use antares_model::NgsiError;
use antares_store::filter::{EntityFilter, Page};
use axum::extract::{Path, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use serde_json::{json, Value};
use std::collections::{BTreeMap, BTreeSet};

use crate::negotiate::CleanParams;

/// Which of the tenant's entities a fold needs. 5.7.5/5.7.8 describe the
/// types and attributes "for which entity instances exist within the NGSI-LD
/// system", so those two fold the whole tenant; 5.7.7 asks about ONE entity
/// type and 5.7.10 about ONE attribute, and an entity carrying neither can
/// contribute nothing to either answer.
#[derive(Clone, Copy)]
enum Narrow<'a> {
    Tenant,
    Type(&'a str),
    Attr(&'a str),
}

impl Narrow<'_> {
    /// The same predicate the datastore compiles, re-applied when the backend
    /// reports it did not (`decided == false`): that arm returns the tenant's
    /// rows unfiltered, and cutting THOSE at the ceiling would answer 5.7.7
    /// with ResourceNotFound for a type whose entities merely sort past it.
    fn matches(self, doc: &Value) -> bool {
        match self {
            Narrow::Tenant => true,
            // `types @> ARRAY[t]`
            Narrow::Type(t) => doc["type"]
                .as_array()
                .is_some_and(|a| a.iter().any(|v| v.as_str() == Some(t))),
            // `entity ?| ARRAY[a]`
            Narrow::Attr(a) => doc.get(a).is_some(),
        }
    }
}

/// The entities a fold reads at once. A fold used to read all of them in one
/// statement, up to the 100 000 of `ANTARES_DISCOVERY_SCAN_MAX`: a `/types`
/// over a 25 000-entity tenant held every document at once and took the
/// broker past its 1536 MiB limit within seconds of a start (T-4562). Page by
/// page, a fold holds one page and the sets it folds into.
const SCAN_PAGE: usize = 500;

/// Hands `each` at most `max` of the entities the fold needs, as the caller's
/// narrowing shows them, and answers whether more exist. The narrowing and
/// the page are pushed into the datastore, so on a backend that takes them
/// the ceiling bounds the rows READ and it bounds them over the narrowed set:
/// a 5.7.7 request about one type reads that type, not the tenant. A backend
/// that ignores the page (`paged == false`) hands back its own row set, and
/// the predicate above plus the truncation here reproduce the same answer.
/// Asking for one row past what is still wanted is what makes the overflow
/// visible; `total`, when the backend reports it, is the exact pre-page count.
async fn scan_each(
    st: &AppState,
    tenant: &antares_model::TenantId,
    max: usize,
    only: Narrow<'_>,
    view: View<'_>,
    page: usize,
    mut each: impl FnMut(Value),
) -> Result<bool, NgsiError> {
    let groups: Vec<Vec<String>> = match only {
        Narrow::Type(t) => vec![vec![t.to_owned()]],
        _ => Vec::new(),
    };
    let attrs: Vec<String> = match only {
        Narrow::Attr(a) => vec![a.to_owned()],
        _ => Vec::new(),
    };
    let mut offset = 0usize;
    loop {
        let want = max.saturating_sub(offset).min(page.max(1));
        let f = EntityFilter {
            types: (!groups.is_empty()).then_some(&groups[..]),
            attrs: (!attrs.is_empty()).then_some(&attrs[..]),
            page: Some(Page {
                offset: i64::try_from(offset).unwrap_or(i64::MAX),
                limit: i64::try_from(want.saturating_add(1)).unwrap_or(i64::MAX),
                count: false,
            }),
            ..Default::default()
        };
        let out = st.store.query_entities(tenant, &f).await?;
        let mut rows = out.rows;
        let more = if out.paged {
            rows.len() > want
        } else {
            // the whole (narrowed) set came back at once: the ceiling is cut here
            if !out.decided {
                rows.retain(|d| only.matches(d));
            }
            let more = out.total.map_or(rows.len() > max, |t| {
                t > i64::try_from(max).unwrap_or(i64::MAX)
            });
            rows.truncate(max);
            visible(st, tenant, view, rows, &mut each).await;
            return Ok(more);
        };
        rows.truncate(want);
        offset = offset.saturating_add(rows.len());
        visible(st, tenant, view, rows, &mut each).await;
        if !more || want == 0 {
            return Ok(more);
        }
        if offset >= max {
            return Ok(true);
        }
    }
}

/// ADR-0023 rule 5: discovery answers from what the narrowing selects.
/// ponytail: the ceiling counts rows before the narrowing, so a narrowed fold
/// can stop short of `max` visible rows; push q into the filter if that shows up
async fn visible(
    st: &AppState,
    tenant: &antares_model::TenantId,
    view: View<'_>,
    rows: Vec<Value>,
    each: &mut impl FnMut(Value),
) {
    let Some((r, ctx)) = view else {
        rows.into_iter().for_each(each);
        return;
    };
    for d in rows {
        if hides_a_type(r, &d)
            || !crate::repr::policy_admits(st, tenant, ctx, r.policy_reach.as_deref(), &d).await
        {
            continue;
        }
        each(crate::repr::policy_view(&d, r));
    }
}

/// The rows of [`scan_each`] collected, for the tests that look at them.
#[cfg(test)]
async fn scan(
    st: &AppState,
    tenant: &antares_model::TenantId,
    max: usize,
    only: Narrow<'_>,
    view: View<'_>,
) -> Result<(Vec<Value>, bool), NgsiError> {
    let mut rows = Vec::new();
    let more = scan_each(st, tenant, max, only, view, 2, |d| rows.push(d)).await?;
    Ok((rows, more))
}

/// The caller's narrowing and the @context its q is read with; `None` when
/// the engine narrowed nothing.
type View<'a> = Option<(&'a crate::repr::Repr, &'a antares_jsonld::Context)>;

/// Whether `omit` names one of the Entity's types. ADR-0023 rule 5 reads
/// `pick`/`omit` as type and Attribute names here. `omit` hides the
/// Entities of a type it names. `pick` keeps the Attributes it names, and,
/// as on a read, never hides the Entity frame.
fn hides_a_type(r: &crate::repr::Repr, doc: &Value) -> bool {
    let Some(omit) = &r.policy_omit else {
        return false;
    };
    doc["type"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .any(|t| omit.iter().any(|n| n.iri == t || n.raw == t))
}

/// The narrowing a discovery handler applies, from the engine's Filter.
fn view_of(filter: &crate::policy::Filter) -> Option<crate::repr::Repr> {
    (!filter.is_empty()).then(|| crate::repr::narrowing(filter))
}

/// A fold that hit the scan ceiling answered from a prefix of the tenant's
/// entities: the list is a subset and a by-name lookup can miss. IETF RFC
/// 7234 5.5.1 warn-code 199 (Miscellaneous Warning) carries that fact to the
/// client in the `NGSILD-Warning` header of 6.3.17.
fn mark_partial(resp: &mut Response, partial: bool, alias: &str) {
    if partial {
        crate::paging::attach_warnings(
            resp,
            &[crate::federation::warning(
                199,
                alias,
                "entity scan ceiling reached; the discovery result is incomplete",
            )],
        );
    }
}

use antares_model::is_meta;

/// type IRI → (entity count, attr IRI → attribute types seen)
type TypeStats = BTreeMap<String, (usize, BTreeMap<String, BTreeSet<String>>)>;

/// A datastore failure is an InternalError (Table 6.3.2-1), never an empty
/// fold — reporting "no such type" because the query failed would be a lie.
/// The second return member is true when the fold stopped at `max`, i.e. the
/// answer is a subset of the tenant's types.
async fn type_stats(
    st: &AppState,
    tenant: &antares_model::TenantId,
    max: usize,
    only: Narrow<'_>,
    view: View<'_>,
) -> Result<(TypeStats, bool), NgsiError> {
    let mut map: TypeStats = BTreeMap::new();
    let partial = scan_each(st, tenant, max, only, view, SCAN_PAGE, |doc| {
        let mut attrs: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
        if let Some(o) = doc.as_object() {
            for (k, v) in o {
                if is_meta(k) {
                    continue;
                }
                let types: BTreeSet<String> = v
                    .as_array()
                    .map(|arr| {
                        arr.iter()
                            .filter_map(|i| i.get("type").and_then(Value::as_str))
                            .map(str::to_owned)
                            .collect()
                    })
                    .unwrap_or_default();
                attrs.entry(k.clone()).or_default().extend(types);
            }
        }
        for t in doc
            .get("type")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default()
        {
            if let Some(t) = t.as_str() {
                let e = map.entry(t.to_owned()).or_default();
                e.0 = e.0.saturating_add(1);
                for (a, tys) in &attrs {
                    e.1.entry(a.clone())
                        .or_default()
                        .extend(tys.iter().cloned());
                }
            }
        }
    })
    .await?;
    Ok((map, partial))
}

/// The Entities one probe of [`type_names`] reads at a time: a type a view
/// shows at all is usually shown on its first Entity.
const PROBE_PAGE: usize = 20;

/// 5.7.5 without details (T-4610): the types the store's type list names,
/// each kept when the caller sees at least one Entity of it, so the answer is
/// the fold's and the cost is one read of the list plus a probe per type.
/// MCP `list_types` folded every Entity of a 25 000-Entity view for these
/// names, 20 s and the pool. `None` when the backend keeps no type list;
/// `true` beside the names when a probe stopped at `max` Entities read.
async fn type_names(
    st: &AppState,
    tenant: &antares_model::TenantId,
    max: usize,
    view: View<'_>,
) -> Result<Option<(Vec<String>, bool)>, NgsiError> {
    let Some(names) = st.store.entity_type_names(tenant).await? else {
        return Ok(None);
    };
    let Some(view) = view else {
        return Ok(Some((names, false)));
    };
    let mut beside = Vec::new();
    for rule in rules_of(view) {
        beside.push(match rule.types.is_empty() {
            true => None,
            false => st
                .store
                .entity_types_beside(tenant, &rule.types)
                .await?
                .map(|t| t.into_iter().collect::<BTreeSet<String>>()),
        });
    }
    let mut shown = Vec::new();
    let mut partial = false;
    for name in names {
        match first_visible(st, tenant, max, &name, view, &beside).await? {
            Probe::Seen => shown.push(name),
            Probe::None => {}
            Probe::Cut => partial = true,
        }
    }
    Ok(Some((shown, partial)))
}

/// What a probe of one type found.
enum Probe {
    Seen,
    None,
    Cut,
}

/// The rules a probe follows: a linked term never holds (ADR-0023 rule 11),
/// so a rule with one shows nothing and is not probed.
fn rules_of<'a>(
    view: (&'a crate::repr::Repr, &antares_jsonld::Context),
) -> impl Iterator<Item = &'a crate::policy::Rule> {
    view.0
        .policy_types
        .as_deref()
        .map_or(&[][..], |f| &f.rules[..])
        .iter()
        .filter(|r| r.q.as_deref().is_none_or(|q| !q.has_linked_paths()))
}

/// Whether the caller sees an Entity of `name`. Each rule of the narrowing
/// is one probe, the rule's `q` pushed down, so a condition few Entities meet
/// reads those few. A rule naming `name` probes `name` alone; a
/// rule naming other types probes `name` beside each of them, and only when
/// `beside` (per rule, from the store) says an Entity carries both: on
/// Postgres such a two-type group with no match walked the Tenant's whole
/// primary key for its `ORDER BY id LIMIT` (T-4683, 17 s for `list_types`).
/// A narrowing without rules is one probe of `name`. [`visible`] still judges
/// every Entity a probe reads.
async fn first_visible(
    st: &AppState,
    tenant: &antares_model::TenantId,
    max: usize,
    name: &str,
    view: (&crate::repr::Repr, &antares_jsonld::Context),
    beside: &[Option<BTreeSet<String>>],
) -> Result<Probe, NgsiError> {
    let alone = || vec![vec![name.to_owned()]];
    let mut probes: Vec<(Vec<Vec<String>>, Option<&antares_ql::QNode>)> = Vec::new();
    for (i, r) in rules_of(view).enumerate() {
        let groups = if r.types.iter().any(|t| t == name) {
            alone()
        } else if r.types.is_empty()
            || beside
                .get(i)
                .and_then(Option::as_ref)
                .is_some_and(|b| !b.contains(name))
        {
            continue;
        } else {
            r.types
                .iter()
                .map(|t| vec![name.to_owned(), t.clone()])
                .collect()
        };
        probes.push((groups, r.q.as_deref()));
    }
    if view
        .0
        .policy_types
        .as_deref()
        .is_none_or(|f| f.rules.is_empty())
    {
        probes.push((alone(), None));
    }
    let mut cut = false;
    for (groups, q) in &probes {
        match probe(st, tenant, max, name, groups, *q, view).await? {
            Probe::Seen => return Ok(Probe::Seen),
            Probe::Cut => cut = true,
            Probe::None => {}
        }
    }
    Ok(if cut { Probe::Cut } else { Probe::None })
}

/// One probe of [`first_visible`], read [`PROBE_PAGE`] at a time up to `max`.
async fn probe(
    st: &AppState,
    tenant: &antares_model::TenantId,
    max: usize,
    name: &str,
    groups: &[Vec<String>],
    q: Option<&antares_ql::QNode>,
    view: (&crate::repr::Repr, &antares_jsonld::Context),
) -> Result<Probe, NgsiError> {
    let ctx = crate::repr::policy_context();
    let expand = |t: &str| ctx.expand_key(t);
    let only = Narrow::Type(name);
    let mut offset = 0usize;
    while offset < max {
        let want = PROBE_PAGE.min(max.saturating_sub(offset));
        let f = EntityFilter {
            types: Some(groups),
            q,
            expand: &expand,
            ctx: Some(ctx),
            page: Some(Page {
                offset: i64::try_from(offset).unwrap_or(i64::MAX),
                limit: i64::try_from(want).unwrap_or(i64::MAX),
                count: false,
            }),
            ..Default::default()
        };
        let out = st.store.query_entities(tenant, &f).await?;
        let mut rows = out.rows;
        if !out.decided {
            rows.retain(|d| only.matches(d));
        }
        // a backend that ignores the page answered the whole narrowed set
        let last = !out.paged || rows.len() < want;
        rows.truncate(max.saturating_sub(offset));
        offset = offset.saturating_add(if out.paged { want } else { max });
        let mut seen = false;
        visible(st, tenant, Some(view), rows, &mut |_| seen = true).await;
        if seen {
            return Ok(Probe::Seen);
        }
        if last {
            return Ok(Probe::None);
        }
    }
    Ok(Probe::Cut)
}

/// attr IRI → (count, attribute types, entity type IRIs)
type AttrStats = BTreeMap<String, (usize, BTreeSet<String>, BTreeSet<String>)>;

/// Same bound and same incompleteness report as `type_stats`.
async fn attr_stats(
    st: &AppState,
    tenant: &antares_model::TenantId,
    max: usize,
    only: Narrow<'_>,
    view: View<'_>,
) -> Result<(AttrStats, bool), NgsiError> {
    let mut map: AttrStats = BTreeMap::new();
    let partial = scan_each(st, tenant, max, only, view, SCAN_PAGE, |doc| {
        let etypes: Vec<String> = doc
            .get("type")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default()
            .iter()
            .filter_map(Value::as_str)
            .map(str::to_owned)
            .collect();
        if let Some(o) = doc.as_object() {
            for (k, v) in o {
                if is_meta(k) {
                    continue;
                }
                let e = map.entry(k.clone()).or_default();
                // Table 5.2.28-1 attributeCount: "Number of attribute
                // instances with this attribute name" — a multi-instance
                // attribute (4.5.5 datasetId) counts once per instance.
                e.0 = e.0.saturating_add(v.as_array().map_or(1, Vec::len));
                if let Some(arr) = v.as_array() {
                    for inst in arr {
                        if let Some(t) = inst.get("type").and_then(Value::as_str) {
                            e.1.insert(t.to_owned());
                        }
                    }
                }
                e.2.extend(etypes.iter().cloned());
            }
        }
    })
    .await?;
    Ok((map, partial))
}

// ---------- GET /types (5.7.5/5.7.6) ----------

/// 4.5.10 Entity Type List Representation, members per Table 5.2.24-1
/// (5.2.24 EntityTypeList): id a valid URI, type equal to "EntityTypeList",
/// typeList the entity type names — with details=true the 4.5.11 detailed
/// list of Table 5.2.25-1 EntityType objects (id = type FQN, type
/// "EntityType", attributeNames, typeName) (5.7.5/5.7.6).
pub async fn entity_types(
    State(st): State<AppState>,
    CleanParams(params): CleanParams,
    headers: HeaderMap,
) -> Response {
    let go = async {
        let tenant = tenant_from(&headers)?;
        check_params(&params, &["details", "local", "count"])?;
        let accept = parse_accept(&headers)?;
        let ctx = request_context(&st.loader, &headers).await?;
        let filter = gate!(st, &tenant, &headers, "5.7.5").await?;
        let view = view_of(&filter);
        let view = view.as_ref().map(|r| (r, &*ctx));
        let details = params.get("details").map(String::as_str) == Some("true");
        let max = *crate::bounds::MAX_FOLD_DOCS;
        // The names alone come from the store's type list (T-4610); the
        // details, and a backend without that list, fold the Entities.
        let names = match details {
            true => None,
            false => type_names(&st, &tenant, max, view).await?,
        };
        let (stats, partial) = match names {
            Some((names, partial)) => (
                names.into_iter().map(|t| (t, Default::default())).collect(),
                partial,
            ),
            None => type_stats(&st, &tenant, max, Narrow::Tenant, view).await?,
        };
        let payload = if details {
            Value::Array(
                stats
                    .iter()
                    .map(|(t, (_, attrs))| {
                        json!({
                            "id": t,
                            "type": "EntityType",
                            "typeName": ctx.compact_iri(t),
                            "attributeNames": attrs.keys().map(|a| ctx.compact_iri(a)).collect::<Vec<_>>(),
                        })
                    })
                    .collect(),
            )
        } else {
            json!({
                "id": format!("urn:ngsi-ld:EntityTypeList:{}", uuid::Uuid::new_v4()),
                "type": "EntityTypeList",
                "typeList": stats.keys().map(|t| ctx.compact_iri(t)).collect::<Vec<_>>(),
            })
        };
        let mut resp = respond(StatusCode::OK, payload, &ctx, accept, &tenant);
        mark_partial(&mut resp, partial, &st.host_alias);
        filter.mark(&st.host_alias, resp.headers_mut());
        Ok::<_, ApiError>(resp)
    };
    go.await.unwrap_or_else(|e| e.into_response())
}

// ---------- GET /types/{type} (5.7.7) ----------

/// 4.5.12 Entity Type Information Representation, members per Table
/// 5.2.26-1 (5.2.26 EntityTypeInfo): id = the entity type FQN, fixed type
/// "EntityTypeInfo", typeName (short name under the @context), entityCount
/// an unsigned integer, attributeDetails Attribute[] restricted to the
/// elements id/type/attributeName/attributeTypes (5.7.7).
pub async fn entity_type_info(
    State(st): State<AppState>,
    Path(type_name): Path<String>,
    CleanParams(params): CleanParams,
    headers: HeaderMap,
) -> Response {
    let go = async {
        let tenant = tenant_from(&headers)?;
        check_params(&params, &["local"])?;
        let accept = parse_accept(&headers)?;
        let ctx = request_context(&st.loader, &headers).await?;
        let iri = ctx.expand_key(&type_name);
        let filter =
            gate!(st, &tenant, &headers, "5.7.7", types: std::slice::from_ref(&iri)).await?;
        let view = view_of(&filter);
        // Only instances of this type carry its entityCount and its
        // attributeDetails (5.2.26), so the fold asks the datastore for those
        // and the ceiling applies to them — a tenant of 100M entities with 50
        // Buildings answers exactly, instead of from the tenant's first page.
        let (stats, partial) = type_stats(
            &st,
            &tenant,
            *crate::bounds::MAX_FOLD_DOCS,
            Narrow::Type(&iri),
            view.as_ref().map(|r| (r, &*ctx)),
        )
        .await?;
        let Some((count, attrs)) = stats.get(&iri) else {
            let mut resp = ApiError::from(NgsiError::ResourceNotFound(format!(
                "no entities of type {type_name}"
            )))
            .into_response();
            mark_partial(&mut resp, partial, &st.host_alias);
            return Ok(resp);
        };
        let attr_details: Vec<Value> = attrs
            .iter()
            .map(|(a, atypes)| {
                json!({
                    "id": a,
                    "type": "Attribute",
                    "attributeName": ctx.compact_iri(a),
                    "attributeTypes": atypes.iter().cloned().collect::<Vec<_>>(),
                })
            })
            .collect();
        let payload = json!({
            "id": iri,
            "type": "EntityTypeInfo",
            "typeName": ctx.compact_iri(&iri),
            "entityCount": count,
            "attributeDetails": attr_details,
        });
        let mut resp = respond(StatusCode::OK, payload, &ctx, accept, &tenant);
        mark_partial(&mut resp, partial, &st.host_alias);
        filter.mark(&st.host_alias, resp.headers_mut());
        Ok::<_, ApiError>(resp)
    };
    go.await.unwrap_or_else(|e| e.into_response())
}

// ---------- GET /attributes (5.7.8/5.7.9) ----------

/// 4.5.13 Attribute List Representation, members per Table 5.2.27-1
/// (5.2.27 AttributeList): id a valid URI, type "AttributeList",
/// attributeList of attribute names — with details=true the 4.5.14 detailed
/// list of Table 5.2.28-1 Attribute objects (id = attribute URI, type
/// "Attribute", attributeName, typeNames).
pub async fn attributes(
    State(st): State<AppState>,
    CleanParams(params): CleanParams,
    headers: HeaderMap,
) -> Response {
    let go = async {
        let tenant = tenant_from(&headers)?;
        check_params(&params, &["details", "local", "count"])?;
        let accept = parse_accept(&headers)?;
        let ctx = request_context(&st.loader, &headers).await?;
        let filter = gate!(st, &tenant, &headers, "5.7.8").await?;
        let view = view_of(&filter);
        let (stats, partial) = attr_stats(
            &st,
            &tenant,
            *crate::bounds::MAX_FOLD_DOCS,
            Narrow::Tenant,
            view.as_ref().map(|r| (r, &*ctx)),
        )
        .await?;
        let details = params.get("details").map(String::as_str) == Some("true");
        let payload = if details {
            Value::Array(
                stats
                    .iter()
                    .map(|(a, (_, _, etypes))| {
                        json!({
                            "id": a,
                            "type": "Attribute",
                            "attributeName": ctx.compact_iri(a),
                            "typeNames": etypes.iter().map(|t| ctx.compact_iri(t)).collect::<Vec<_>>(),
                        })
                    })
                    .collect(),
            )
        } else {
            json!({
                "id": format!("urn:ngsi-ld:AttributeList:{}", uuid::Uuid::new_v4()),
                "type": "AttributeList",
                "attributeList": stats.keys().map(|a| ctx.compact_iri(a)).collect::<Vec<_>>(),
            })
        };
        let mut resp = respond(StatusCode::OK, payload, &ctx, accept, &tenant);
        mark_partial(&mut resp, partial, &st.host_alias);
        filter.mark(&st.host_alias, resp.headers_mut());
        Ok::<_, ApiError>(resp)
    };
    go.await.unwrap_or_else(|e| e.into_response())
}

// ---------- GET /attributes/{attrId} (5.7.10) ----------

/// 4.5.15 Attribute Information Representation, members per Table 5.2.28-1
/// (5.2.28 Attribute): id = the attribute URI, fixed type "Attribute",
/// attributeName (short name under @context), plus the optional
/// attributeCount (unsigned integer) / attributeTypes / typeNames members.
pub async fn attribute_info(
    State(st): State<AppState>,
    Path(attr): Path<String>,
    CleanParams(params): CleanParams,
    headers: HeaderMap,
) -> Response {
    let go = async {
        let tenant = tenant_from(&headers)?;
        check_params(&params, &["local"])?;
        let accept = parse_accept(&headers)?;
        let ctx = request_context(&st.loader, &headers).await?;
        // 5.7.10 / Table 6.28.2-1: the FQN or a shortname the request
        // @context defines. Discovery reads the tenant's attribute
        // inventory and touches no stored document, so a name that
        // expands to nothing an Entity carries is simply absent —
        // ResourceNotFound below, not a rejected name.
        let iri = ctx.expand_key(&attr);
        let filter =
            gate!(st, &tenant, &headers, "5.7.10", attrs: std::slice::from_ref(&iri)).await?;
        let view = view_of(&filter);
        // attributeCount, attributeTypes and typeNames (5.2.28) are all
        // properties of the entities that CARRY this attribute; the rest of
        // the tenant is not read.
        let (stats, partial) = attr_stats(
            &st,
            &tenant,
            *crate::bounds::MAX_FOLD_DOCS,
            Narrow::Attr(&iri),
            view.as_ref().map(|r| (r, &*ctx)),
        )
        .await?;
        let Some((count, attr_types, etypes)) = stats.get(&iri) else {
            let mut resp = ApiError::from(NgsiError::ResourceNotFound(format!(
                "attribute {attr} not found"
            )))
            .into_response();
            mark_partial(&mut resp, partial, &st.host_alias);
            return Ok(resp);
        };
        let payload = json!({
            "id": iri,
            "type": "Attribute",
            "attributeName": ctx.compact_iri(&iri),
            "attributeCount": count,
            "attributeTypes": attr_types.iter().cloned().collect::<Vec<_>>(),
            "typeNames": etypes.iter().map(|t| ctx.compact_iri(t)).collect::<Vec<_>>(),
        });
        let mut resp = respond(StatusCode::OK, payload, &ctx, accept, &tenant);
        mark_partial(&mut resp, partial, &st.host_alias);
        filter.mark(&st.host_alias, resp.headers_mut());
        Ok::<_, ApiError>(resp)
    };
    go.await.unwrap_or_else(|e| e.into_response())
}

#[cfg(test)]
mod discovery_folds {
    use super::*;
    use crate::state::AppState;
    use antares_model::TenantId;
    use antares_store::Kind;
    use serde_json::json;

    const V: &str = "https://uri.etsi.org/ngsi-ld/default-context/v";
    const R: &str = "https://uri.etsi.org/ngsi-ld/default-context/r";
    const BUILDING: &str = "https://uri.etsi.org/ngsi-ld/default-context/Building";
    const SENSOR: &str = "https://uri.etsi.org/ngsi-ld/default-context/Sensor";

    /// A ceiling no test fixture reaches, so the fold sees every entity.
    const ALL: usize = 1_000;

    fn tid(t: &str) -> TenantId {
        TenantId::new(t).expect("tenant")
    }

    async fn seed(st: &AppState, tenant: &str, id: &str, doc: Value) {
        assert!(st
            .store
            .create(&tid(tenant), Kind::Entity, id, doc)
            .await
            .expect("create"));
    }

    /// 5.2.26: entityCount is the number of entity instances of the type;
    /// 5.2.25 attributeNames lists the attributes those instances can have —
    /// Entity members are not among them.
    #[tokio::test]
    async fn type_stats_folds_types_and_attribute_names() {
        let st = AppState::new("test".into());
        seed(
            &st,
            "ta",
            "urn:ngsi-ld:B:1",
            json!({
                "id": "urn:ngsi-ld:B:1",
                "type": [BUILDING, SENSOR],
                "createdAt": "2026-01-01T00:00:00Z",
                "modifiedAt": "2026-01-01T00:00:00Z",
                "scope": ["/a/b"],
                V: [{"type": "Property", "value": 1}],
                R: [{"type": "Relationship", "object": "urn:ngsi-ld:B:2"}],
            }),
        )
        .await;
        seed(
            &st,
            "ta",
            "urn:ngsi-ld:B:2",
            json!({
                "id": "urn:ngsi-ld:B:2",
                "type": [BUILDING],
                V: [{"type": "GeoProperty", "value": {"type": "Point", "coordinates": [0, 0]}}],
            }),
        )
        .await;
        let (stats, partial) = type_stats(&st, &tid("ta"), ALL, Narrow::Tenant, None)
            .await
            .expect("stats");
        assert!(!partial, "two entities are under any sane ceiling");
        let (count, attrs) = stats.get(BUILDING).expect("Building");
        assert_eq!(*count, 2, "two entities carry the Building type");
        assert_eq!(stats.get(SENSOR).expect("Sensor").0, 1);
        assert!(attrs.contains_key(V) && attrs.contains_key(R));
        for meta in ["id", "type", "scope", "createdAt", "modifiedAt"] {
            assert!(
                !attrs.contains_key(meta),
                "{meta} must not be reported as an attribute name"
            );
        }
        // 5.2.28 attributeTypes: every attribute type an instance carried
        assert!(attrs[V].contains("Property") && attrs[V].contains("GeoProperty"));
    }

    /// One shared datastore, one tenant per request: an entity of another
    /// tenant contributes to no fold (4.15 multi-tenancy).
    #[tokio::test]
    async fn stats_are_tenant_scoped() {
        let st = AppState::new("test".into());
        seed(
            &st,
            "ta",
            "urn:ngsi-ld:B:1",
            json!({
                "id": "urn:ngsi-ld:B:1",
                "type": [BUILDING],
                V: [{"type": "Property", "value": 1}],
            }),
        )
        .await;
        for other in ["tb", TenantId::DEFAULT] {
            let t = tid(other);
            assert!(
                type_stats(&st, &t, ALL, Narrow::Tenant, None)
                    .await
                    .expect("stats")
                    .0
                    .is_empty(),
                "{other} must not see another tenant's types"
            );
            assert!(
                attr_stats(&st, &t, ALL, Narrow::Tenant, None)
                    .await
                    .expect("stats")
                    .0
                    .is_empty(),
                "{other} must not see another tenant's attributes"
            );
        }
        assert!(type_stats(&st, &tid("ta"), ALL, Narrow::Tenant, None)
            .await
            .expect("stats")
            .0
            .contains_key(BUILDING));
    }

    /// Table 5.2.28-1: attributeCount is the "number of attribute instances
    /// with this attribute name" — multi-instance attributes (4.5.5
    /// datasetId) count once per instance, not once per entity.
    #[tokio::test]
    async fn attr_stats_counts_attribute_instances() {
        let st = AppState::new("test".into());
        seed(
            &st,
            "ta",
            "urn:ngsi-ld:B:1",
            json!({
                "id": "urn:ngsi-ld:B:1",
                "type": [BUILDING],
                V: [
                    {"type": "Property", "value": 1,
                     "datasetId": "urn:ngsi-ld:ds:1"},
                    {"type": "Property", "value": 2,
                     "datasetId": "urn:ngsi-ld:ds:2"},
                ],
            }),
        )
        .await;
        seed(
            &st,
            "ta",
            "urn:ngsi-ld:B:2",
            json!({
                "id": "urn:ngsi-ld:B:2",
                "type": [SENSOR],
                V: [{"type": "Property", "value": 3}],
            }),
        )
        .await;
        let (stats, _) = attr_stats(&st, &tid("ta"), ALL, Narrow::Tenant, None)
            .await
            .expect("stats");
        let (count, atypes, etypes) = stats.get(V).expect("v");
        assert_eq!(*count, 3, "two instances on B:1 plus one on B:2");
        assert!(atypes.contains("Property"));
        assert_eq!(etypes.len(), 2, "both entity types are reported");
        assert!(
            !stats.contains_key("createdAt") && !stats.contains_key("id"),
            "Entity members must not appear in the attribute fold"
        );
    }

    /// 4.8 expiresAt: an expired entity no longer exists, and an expired
    /// attribute instance is gone with it — neither is discoverable.
    #[tokio::test]
    async fn expired_entities_and_attributes_are_not_discoverable() {
        let st = AppState::new("test".into());
        seed(
            &st,
            "ta",
            "urn:ngsi-ld:B:1",
            json!({
                "id": "urn:ngsi-ld:B:1",
                "type": [BUILDING],
                "expiresAt": "2000-01-01T00:00:00Z",
                V: [{"type": "Property", "value": 1}],
            }),
        )
        .await;
        seed(
            &st,
            "ta",
            "urn:ngsi-ld:B:2",
            json!({
                "id": "urn:ngsi-ld:B:2",
                "type": [SENSOR],
                V: [{"type": "Property", "value": 1,
                     "expiresAt": "2000-01-01T00:00:00Z"}],
                R: [{"type": "Relationship", "object": "urn:ngsi-ld:B:1"}],
            }),
        )
        .await;
        let (stats, _) = type_stats(&st, &tid("ta"), ALL, Narrow::Tenant, None)
            .await
            .expect("stats");
        assert!(
            !stats.contains_key(BUILDING),
            "the expired entity must not appear in the type list"
        );
        let (count, attrs) = stats.get(SENSOR).expect("Sensor");
        assert_eq!(*count, 1);
        assert!(
            !attrs.contains_key(V),
            "the expired attribute instance must not appear"
        );
        assert!(attrs.contains_key(R));
        assert!(!attr_stats(&st, &tid("ta"), ALL, Narrow::Tenant, None)
            .await
            .expect("stats")
            .0
            .contains_key(V));
    }

    /// A tenant with no entities has no types and no attributes — an empty
    /// fold, not an error, and never "incomplete".
    #[tokio::test]
    async fn empty_tenant_folds_to_nothing() {
        let st = AppState::new("test".into());
        for t in ["ta", TenantId::DEFAULT] {
            let (types, partial) = type_stats(&st, &tid(t), ALL, Narrow::Tenant, None)
                .await
                .expect("stats");
            assert!(types.is_empty() && !partial);
            let (attrs, partial) = attr_stats(&st, &tid(t), ALL, Narrow::Tenant, None)
                .await
                .expect("stats");
            assert!(attrs.is_empty() && !partial);
            let (rows, partial) = scan(&st, &tid(t), ALL, Narrow::Tenant, None)
                .await
                .expect("scan");
            assert!(rows.is_empty() && !partial);
        }
    }

    /// The fold reads at most `max` entities and says so when the tenant
    /// holds more, instead of silently answering from a prefix.
    #[tokio::test]
    async fn scan_stops_at_the_ceiling_and_reports_it() {
        let st = AppState::new("test".into());
        for i in 0..5 {
            let id = format!("urn:ngsi-ld:B:{i}");
            let mut doc = serde_json::Map::new();
            doc.insert("id".into(), Value::String(id.clone()));
            doc.insert("type".into(), json!([format!("{BUILDING}{i}")]));
            doc.insert(format!("{V}{i}"), json!([{"type": "Property", "value": i}]));
            seed(&st, "ta", &id, Value::Object(doc)).await;
        }
        for (max, want) in [(1, 1), (4, 4), (5, 5), (6, 5)] {
            let (rows, partial) = scan(&st, &tid("ta"), max, Narrow::Tenant, None)
                .await
                .expect("scan");
            assert_eq!(rows.len(), want, "max {max}");
            assert_eq!(partial, max < 5, "max {max} incompleteness");
        }
        // the folds inherit the bound: one entity read, one type, one attr
        let (types, partial) = type_stats(&st, &tid("ta"), 1, Narrow::Tenant, None)
            .await
            .expect("stats");
        assert_eq!(types.len(), 1, "the fold read past its ceiling");
        assert!(partial, "a truncated type fold must report itself");
        let (attrs, partial) = attr_stats(&st, &tid("ta"), 1, Narrow::Tenant, None)
            .await
            .expect("stats");
        assert_eq!(attrs.len(), 1, "the fold read past its ceiling");
        assert!(partial, "a truncated attribute fold must report itself");
        // at the exact size the answer is complete, so nothing is flagged
        let (types, partial) = type_stats(&st, &tid("ta"), 5, Narrow::Tenant, None)
            .await
            .expect("stats");
        assert_eq!(types.len(), 5);
        assert!(!partial);
    }

    /// A complete answer carries no warning; a truncated one carries exactly
    /// one 199 (RFC 7234 5.5.1) naming this broker.
    #[test]
    fn mark_partial_emits_one_199_only_when_truncated() {
        let mut resp = StatusCode::OK.into_response();
        mark_partial(&mut resp, false, "broker-a");
        assert!(resp.headers().get("NGSILD-Warning").is_none());
        mark_partial(&mut resp, true, "broker-a");
        let vals: Vec<_> = resp.headers().get_all("NGSILD-Warning").iter().collect();
        assert_eq!(vals.len(), 1);
        let v = vals[0].to_str().expect("ascii");
        assert!(v.starts_with("199 broker-a \"") && v.ends_with('"'), "{v}");
        assert!(v.contains("incomplete"), "{v}");
    }

    /// The narrowing predicate selects exactly what the datastore's
    /// `types @> ARRAY[t]` / `entity ?| ARRAY[a]` select, and nothing else.
    #[test]
    fn narrow_selects_only_its_own_entities() {
        let doc = json!({
            "id": "urn:ngsi-ld:B:1",
            "type": [BUILDING],
            V: [{"type": "Property", "value": 1}],
        });
        assert!(Narrow::Tenant.matches(&doc));
        assert!(Narrow::Tenant.matches(&json!({})));
        assert!(Narrow::Type(BUILDING).matches(&doc));
        assert!(Narrow::Attr(V).matches(&doc));
        // the negatives: a type this entity does not carry, an attribute it
        // does not carry, a prefix of either, and a typeless document
        assert!(!Narrow::Type(SENSOR).matches(&doc));
        assert!(!Narrow::Attr(R).matches(&doc));
        assert!(!Narrow::Type("https://uri.etsi.org/ngsi-ld/default-context/Build").matches(&doc));
        assert!(!Narrow::Type(BUILDING).matches(&json!({"id": "urn:ngsi-ld:B:2"})));
        assert!(!Narrow::Attr(V).matches(&json!({"type": [BUILDING]})));
    }

    /// 5.7.7/5.7.10 name ONE type / ONE attribute, so the ceiling applies to
    /// the entities that carry it: a type sitting past the ceiling in the
    /// tenant's row order is still found, with an exact entityCount and no
    /// incompleteness warning. Under the tenant-wide fold the same ceiling
    /// hides it — that is the difference the narrowing buys.
    #[tokio::test]
    async fn narrowed_folds_reach_past_the_tenant_wide_ceiling() {
        let st = AppState::new("test".into());
        for i in 0..5 {
            let id = format!("urn:ngsi-ld:B:{i}");
            let mut doc = serde_json::Map::new();
            doc.insert("id".into(), Value::String(id.clone()));
            doc.insert("type".into(), json!([format!("{BUILDING}{i}")]));
            doc.insert(format!("{V}{i}"), json!([{"type": "Property", "value": i}]));
            seed(&st, "ta", &id, Value::Object(doc)).await;
        }
        let last_type = format!("{BUILDING}4");
        let last_attr = format!("{V}4");

        // tenant-wide, ceiling 1: the last entity is not read at all
        let (wide, partial) = type_stats(&st, &tid("ta"), 1, Narrow::Tenant, None)
            .await
            .expect("stats");
        assert!(!wide.contains_key(&last_type), "the ceiling hid the type");
        assert!(partial);

        // narrowed to that type, same ceiling: found, exact, complete
        let (stats, partial) = type_stats(&st, &tid("ta"), 1, Narrow::Type(&last_type), None)
            .await
            .expect("stats");
        let (count, attrs) = stats.get(&last_type).expect("the narrowed type");
        assert_eq!(*count, 1, "one instance carries it");
        assert!(!partial, "the narrowed set fits under the ceiling");
        assert!(attrs.contains_key(&last_attr));
        for i in 0..4 {
            assert!(
                !stats.contains_key(&format!("{BUILDING}{i}")),
                "no other type may be folded into a 5.7.7 answer"
            );
        }

        // and the same for one attribute (5.7.10)
        let (wide, partial) = attr_stats(&st, &tid("ta"), 1, Narrow::Tenant, None)
            .await
            .expect("stats");
        assert!(!wide.contains_key(&last_attr), "the ceiling hid the attr");
        assert!(partial);
        let (stats, partial) = attr_stats(&st, &tid("ta"), 1, Narrow::Attr(&last_attr), None)
            .await
            .expect("stats");
        let (count, atypes, etypes) = stats.get(&last_attr).expect("the narrowed attr");
        assert_eq!(*count, 1);
        assert!(!partial);
        assert!(atypes.contains("Property"));
        assert_eq!(etypes.iter().cloned().collect::<Vec<_>>(), vec![last_type]);
        for i in 0..4 {
            assert!(
                !stats.contains_key(&format!("{V}{i}")),
                "no other attribute may be folded into a 5.7.10 answer"
            );
        }

        // a narrowing that matches nothing is an empty fold, not a prefix of
        // the tenant — this is what makes 5.7.7 answer ResourceNotFound
        let (stats, partial) = type_stats(&st, &tid("ta"), 1, Narrow::Type(SENSOR), None)
            .await
            .expect("stats");
        assert!(stats.is_empty() && !partial);
        let (stats, partial) = attr_stats(&st, &tid("ta"), 1, Narrow::Attr(R), None)
            .await
            .expect("stats");
        assert!(stats.is_empty() && !partial);
    }

    /// A narrowed fold is tenant-scoped like the tenant-wide one (4.15): the
    /// type exists, but only for the tenant that created its instances.
    #[tokio::test]
    async fn narrowed_folds_are_tenant_scoped() {
        let st = AppState::new("test".into());
        seed(
            &st,
            "ta",
            "urn:ngsi-ld:B:1",
            json!({
                "id": "urn:ngsi-ld:B:1",
                "type": [BUILDING],
                V: [{"type": "Property", "value": 1}],
            }),
        )
        .await;
        for other in ["tb", TenantId::DEFAULT] {
            let t = tid(other);
            assert!(
                type_stats(&st, &t, ALL, Narrow::Type(BUILDING), None)
                    .await
                    .expect("stats")
                    .0
                    .is_empty(),
                "{other} must not see another tenant's type"
            );
            assert!(
                attr_stats(&st, &t, ALL, Narrow::Attr(V), None)
                    .await
                    .expect("stats")
                    .0
                    .is_empty(),
                "{other} must not see another tenant's attribute"
            );
        }
        assert_eq!(
            type_stats(&st, &tid("ta"), ALL, Narrow::Type(BUILDING), None)
                .await
                .expect("stats")
                .0
                .get(BUILDING)
                .expect("Building")
                .0,
            1
        );
    }

    /// 5.7.5.4: the answer lists the types "for which entity instances exist
    /// within the NGSI-LD system" — the entity written a moment ago included.
    /// No fold result may be carried over a write.
    #[tokio::test]
    async fn a_freshly_created_type_is_in_the_very_next_fold() {
        let st = AppState::new("test".into());
        let t = tid("ta");
        assert!(type_stats(&st, &t, ALL, Narrow::Tenant, None)
            .await
            .expect("stats")
            .0
            .is_empty());
        seed(
            &st,
            "ta",
            "urn:ngsi-ld:B:1",
            json!({"id": "urn:ngsi-ld:B:1", "type": [BUILDING],
                   V: [{"type": "Property", "value": 1}]}),
        )
        .await;
        let (stats, _) = type_stats(&st, &t, ALL, Narrow::Tenant, None)
            .await
            .expect("stats");
        assert!(stats.contains_key(BUILDING), "the new type must be visible");
        assert!(!stats.contains_key(SENSOR));
        assert!(attr_stats(&st, &t, ALL, Narrow::Tenant, None)
            .await
            .expect("stats")
            .0
            .contains_key(V));
        // a second type on a second write, and a narrowed fold sees it too
        seed(
            &st,
            "ta",
            "urn:ngsi-ld:B:2",
            json!({"id": "urn:ngsi-ld:B:2", "type": [SENSOR],
                   R: [{"type": "Relationship", "object": "urn:ngsi-ld:B:1"}]}),
        )
        .await;
        let (stats, _) = type_stats(&st, &t, ALL, Narrow::Tenant, None)
            .await
            .expect("stats");
        assert!(stats.contains_key(BUILDING) && stats.contains_key(SENSOR));
        assert!(type_stats(&st, &t, ALL, Narrow::Type(SENSOR), None)
            .await
            .expect("stats")
            .0
            .contains_key(SENSOR));
        assert!(attr_stats(&st, &t, ALL, Narrow::Attr(R), None)
            .await
            .expect("stats")
            .0
            .contains_key(R));
        // and a deleted type disappears from the next fold just as fast
        assert!(st
            .store
            .delete(&t, Kind::Entity, "urn:ngsi-ld:B:2")
            .await
            .expect("delete"));
        let (stats, _) = type_stats(&st, &t, ALL, Narrow::Tenant, None)
            .await
            .expect("stats");
        assert!(
            !stats.contains_key(SENSOR),
            "a type with no instances left must not be listed"
        );
    }
}
