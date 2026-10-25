// SPDX-License-Identifier: EUPL-1.2
//! Query temporal evolution (5.7.4) and the batch temporal query (6.24).

use super::*;

// ---------- GET /temporal/entities/ (5.7.4) ----------

pub async fn query_temporal(
    State(st): State<AppState>,
    CleanParams(params): CleanParams,
    headers: HeaderMap,
) -> Response {
    match query_temporal_outer(&st, params, &headers).await {
        Ok(r) => r,
        Err(e) => e.into_response(),
    }
}

/// 5.7.4.4 EntityMap usage on the temporal query: a live map referenced by
/// the NGSILD-EntityMap header fixes the result set to the map's Entities
/// (5.5.14) and its location is echoed; an unknown or expired reference
/// means "a new EntityMap shall be created" (the entityMap=true branch,
/// answering 201 + the fresh location).
pub(super) async fn query_temporal_outer(
    st: &AppState,
    mut params: HashMap<String, String>,
    headers: &HeaderMap,
) -> ApiResult<Response> {
    let tenant = tenant_from(headers)?;
    let named = crate::entities::ReadMembers::of(st, headers, &params).await;
    let filter = gate!(
        st, &tenant, headers, "5.7.4",
        scope_q: params.get("scopeQ").map(String::as_str),
        q: named.q.as_ref(),
        types: &named.types,
        attrs: &named.attrs,
        geo: named.geo.as_ref(),
    )
    .await?;
    let Some(map_ref) = single_header(headers, "NGSILD-EntityMap")? else {
        return query_temporal_inner(st, &params, headers, &filter).await;
    };
    let map_id = map_ref.rsplit('/').next().unwrap_or(&map_ref).to_owned();
    let Some(mut map) = crate::entity_map::map_if_accessible(st, &tenant, headers, &map_id).await
    else {
        params.insert("entityMap".into(), "true".into());
        return query_temporal_inner(st, &params, headers, &filter).await;
    };
    params.remove("entityMap");
    // 5.5.9.3: the map fixes the candidate set and the request's own filters
    // narrow it, so `id=` on this request selects from the map rather than
    // replacing it.
    let candidates = crate::entity_map::candidate_ids(&map, &params);
    // "filters shall be rechecked before returning results" and "Entities not
    // or no longer fitting the query shall be removed from the Entity map
    // during pagination" — so the recheck asks about the map's OWN Entities,
    // in bounded chunks. Asking the whole Tenant instead judged, and then
    // deleted, entries this request never asked about, and lost every
    // candidate past the first page of the recheck. Pruning is judgeable only
    // for "@none" (local) entries: a remote-backed id may merely have an
    // unreachable source right now (5.5.14). Known cost: this recheck is a
    // second temporal query per map-using request, same shape as the entity
    // query's filter re-run.
    let mut matching: std::collections::HashSet<String> = std::collections::HashSet::new();
    for chunk in candidates.chunks(st.max_limit.max(1)) {
        let mut eff = params.clone();
        for k in ["limit", "offset", "count"] {
            eff.remove(k);
        }
        eff.insert("limit".into(), st.max_limit.to_string());
        eff.insert("id".into(), chunk.join(","));
        let resp = query_temporal_inner(st, &eff, headers, &filter).await?;
        let bytes = axum::body::to_bytes(resp.into_body(), st.max_answer_bytes)
            .await
            .map_err(|_| NgsiError::TooManyResults(crate::bounds::ANSWER_TOO_LARGE.into()))?;
        matching.extend(
            serde_json::from_slice::<Value>(&bytes)
                .ok()
                .and_then(|v| v.as_array().cloned())
                .unwrap_or_default()
                .iter()
                .filter_map(|d| d.get("id").and_then(Value::as_str).map(str::to_owned)),
        );
    }
    if let Some(emap) = map.get_mut("entityMap").and_then(Value::as_object_mut) {
        let stale: Vec<String> = candidates
            .iter()
            .filter(|eid| {
                emap.get(eid.as_str())
                    .and_then(Value::as_array)
                    .is_some_and(|a| matches!(a.as_slice(), [only] if only == "@none"))
                    && !matching.contains(eid.as_str())
            })
            .cloned()
            .collect();
        for k in stale {
            emap.remove(&k);
        }
    }
    crate::entity_map::map_put(st, &tenant, map.clone()).await?;
    // fix the query to the candidates that survived the recheck (5.5.14)
    let ids: Vec<&str> = candidates
        .iter()
        .filter(|id| {
            map.get("entityMap")
                .and_then(|m| m.get(id.as_str()))
                .is_some()
        })
        .map(String::as_str)
        .collect();
    params.insert(
        "id".into(),
        if ids.is_empty() {
            "urn:ngsi-ld:entitymap:empty".to_owned()
        } else {
            ids.join(",")
        },
    );
    let mut resp = query_temporal_inner(st, &params, headers, &filter).await?;
    if let Ok(v) = format!("/ngsi-ld/v1/entityMaps/{map_id}").parse() {
        resp.headers_mut().insert("NGSILD-EntityMap", v);
    }
    Ok(resp)
}

/// The 5.7.4.4 preconditions of a temporal query, before any store is
/// touched: the filter must qualify, Linked Entity conditions are not
/// defined for it, a context source filter must parse, and ordering may
/// name only `id` and only where the execution stays local (4.23.1).
pub(super) async fn check_temporal_query(
    st: &AppState,
    params: &HashMap<String, String>,
    headers: &HeaderMap,
    tenant: &TenantId,
    ctx: &Context,
    q_ast: Option<&antares_ql::QNode>,
) -> ApiResult<()> {
    let attrs_qualify = params.get("attrs").is_some_and(|a| {
        a.split(',')
            .any(|n| antares_ql::is_non_system_attr(n.trim()))
    });
    let q_qualifies = q_ast.is_some_and(|ast| {
        ast.attribute_paths()
            .iter()
            .any(|h| antares_ql::is_non_system_attr(h))
    });
    let has_filter = params.contains_key("type")
        || attrs_qualify
        || q_qualifies
        || params.contains_key("georel")
        || params.get("local").map(String::as_str) == Some("true");
    if !has_filter {
        return Err(NgsiError::BadRequestData(
            "temporal query needs at least one of type, attrs, q, georel (5.7.4)".into(),
        )
        .into());
    }
    // 5.7.4.4: Linked Entity retrieval is not defined for temporal queries —
    // linked filter conditions are an unconditional BadRequestData
    if q_ast.map(antares_ql::QNode::max_link_depth).unwrap_or(0) > 0 {
        return Err(NgsiError::BadRequestData(
            "temporal q must not reference Linked Entity attributes (5.7.4.4)".into(),
        )
        .into());
    }
    // 5.7.4.4: a syntactically invalid context source filter is 400; the
    // filter itself gates registrations in federation::reg_matches.
    if let Some(csf) = params.get("csf") {
        parse_q(csf)?;
    }
    crate::paging::check_collation(params)?;
    // 5.7.4.4: temporal ordering may only refer to the "id" entity member,
    // and only where the execution "is limited to the local scope (see
    // clause 5.5.13)" — 4.23.1 gives the reason: "Sort ordering is never
    // applied to distributed operations." The subject is the EXECUTION, so a
    // query nothing would federate to orders without `local=true`.
    if let Some(spec) = params.get("orderBy") {
        if crate::federation::would_federate(st, tenant, ctx, params, headers).await? {
            return Err(NgsiError::BadRequestData(
                "orderBy requires local scope — ordering is never applied to \
                 distributed operations (5.7.4.4, 4.23.1)"
                    .into(),
            )
            .into());
        }
        let non_id = spec.split(',').any(|part| {
            let m = part.trim().split(';').next().unwrap_or("").trim();
            m.split('[').next().unwrap_or(m) != "id"
        });
        if non_id {
            return Err(NgsiError::BadRequestData(
                "temporal orderBy may only name \"id\" (5.7.4.4)".into(),
            )
            .into());
        }
    }
    Ok(())
}

/// 4.5.19: the aggregated answer of a temporal query. The store computed
/// the bucket matrix, so the page is presented with the core members the
/// instance path renders and the aggregated attribute objects copied
/// under their compacted names.
#[allow(clippy::too_many_arguments)]
pub(super) fn respond_aggregated(
    st: &AppState,
    params: &HashMap<String, String>,
    tenant: &TenantId,
    ctx: &Context,
    accept: Accept,
    trepr: &TRepr,
    tq: Option<&TemporalQ>,
    attrs_filter: Option<&Vec<String>>,
    outcome: antares_store::filter::TemporalOutcome,
) -> ApiResult<Response> {
    let total = outcome
        .total
        .map(|t| t as usize)
        .unwrap_or(outcome.rows.len());
    let (page, count_hdr, links) = crate::paging::paginate_pre(
        st,
        params,
        outcome.rows,
        "/ngsi-ld/v1/temporal/entities",
        total,
    )?;
    let timeprop = tq.map_or("observedAt", |t| t.timeproperty.as_str());
    let none = Windowed {
        attrs: Default::default(),
        max_per_attr: 0,
        ts_min: None,
        ts_max: None,
        truncated: false,
        cut_served: None,
        more: false,
        held: Default::default(),
    };
    let mut payload: Vec<Value> = Vec::new();
    for d in &page {
        // core members exactly as the instance path presents them; the
        // aggregated attribute objects are copied under compacted names
        let mut presented = present_temporal(d, &none, ctx, trepr, tq, timeprop)?;
        let Some(out) = presented.as_object_mut() else {
            continue;
        };
        let mut any = false;
        for (k, v) in d.as_object().into_iter().flatten() {
            if is_meta(k) || attrs_filter.is_some_and(|a| !a.contains(k)) {
                continue;
            }
            out.insert(ctx.compact_iri(k), v.clone());
            any = true;
        }
        if any {
            payload.push(presented);
        }
    }
    let mut resp = crate::negotiate::respond_list(StatusCode::OK, payload, ctx, accept, tenant);
    attach_paging(&mut resp, count_hdr, &links);
    Ok(resp)
}

/// The two Table 5.2.33-1 selectors that need no compiled pattern: the
/// type set, and the `attrs` list, which excludes an entity carrying
/// none of the named Attributes. Each drops an entity the store could
/// not narrow away itself.
pub(super) fn type_and_attrs_match(
    doc: &Value,
    types: Option<&Vec<String>>,
    entity_attr_filter: Option<&Vec<String>>,
) -> bool {
    if let Some(types) = types {
        let etypes = doc["type"].as_array().cloned().unwrap_or_default();
        if !etypes
            .iter()
            .any(|t| types.iter().any(|w| Some(w.as_str()) == t.as_str()))
        {
            return false;
        }
    }
    if let Some(want) = entity_attr_filter {
        if !want.iter().any(|a| doc.get(a).is_some()) {
            return false;
        }
    }
    true
}

/// ADR-0023 rule 8, for every narrowing: a history is served while the
/// narrowing's own predicates (its `q`, `scopeQ` and areas) hold for the
/// Entity as it is now, so an Entity that left the narrowing takes its
/// history with it. Its rules' conditions are not read here: rule 11 reads
/// them per instance. An Entity held only temporally (11.2.2) has no state
/// now, and its history `held` is the only state it has: the predicates
/// hold when some instance satisfies them, as the query's window check
/// reads them; `held` absent leaves that to the caller's own check.
pub(super) async fn member_now(
    st: &AppState,
    tenant: &TenantId,
    ctx: &Context,
    filter: &crate::policy::Filter,
    id: &str,
    held: Option<&Value>,
) -> ApiResult<bool> {
    let Some(reach) = crate::repr::narrowing(filter).policy_reach else {
        return Ok(true);
    };
    if reach.q.is_none() && reach.scope_q.is_none() && reach.areas.is_empty() {
        return Ok(true);
    }
    let reach = crate::repr::PolicyReach {
        rules: None,
        ..(*reach).clone()
    };
    match st
        .store
        .get(tenant, antares_store::Kind::Entity, id)
        .await?
    {
        Some(doc) => Ok(crate::repr::policy_admits(st, tenant, ctx, Some(&reach), &doc).await),
        None => Ok(held.is_none_or(|h| {
            let areas: Vec<&antares_ql::geo::GeoQuery> = reach.areas.iter().collect();
            window_conditions_match(h, None, reach.q.as_ref(), &areas, ctx)
                && reach
                    .scope_q
                    .as_deref()
                    .is_none_or(|sq| scope_window_retain(&mut h.clone(), sq, None, "observedAt"))
        })),
    }
}

/// Whether `rules` decide an instance on the Entity's state at its time (a
/// rule with a condition), which [`rules_per_instance`] rebuilds from the
/// whole history. A rule of types, Attributes and windows decides each
/// instance alone, so the store may prune to the query's window (T-4246).
pub(super) fn replays_state(rules: Option<&crate::policy::Filter>) -> bool {
    rules.is_some_and(|r| r.rules.iter().any(crate::policy::Rule::is_conditional))
}

/// Whether the store's per-Attribute cap keeps every instance `rules` would
/// show of the page: with no rules always; under rules only for lastN, when
/// no rule reads state and every window stays open towards now, so the
/// rules keep the newest instances of what they show. A window that closes
/// before now, or firstN, would have the store keep instances the rules
/// then drop (T-4246). Rule windows judge observedAt, so a window under a
/// cap ordered by another `timeproperty` (a backfill: new createdAt, old
/// observedAt) keeps the cap in the API too.
pub(super) fn cap_keeps_shown(
    rules: Option<&crate::policy::Filter>,
    page: &InstancePage,
    timeprop: &str,
) -> bool {
    rules.is_none_or(|r| {
        page.desc
            && !replays_state(Some(r))
            && r.rules.iter().all(|rule| {
                rule.temporal.is_empty()
                    || (timeprop == "observedAt" && rule.temporal.iter().all(|w| w.to.is_none()))
            })
    })
}

/// 5.7.4.4 S2/S3: the values filter and the geoquery are judged on the
/// Attribute instances WITHIN the temporal-query interval, so an
/// out-of-window instance must not satisfy either.
/// ADR-0023 rule 11, temporal per instance: an instance of an Attribute
/// is kept only where a rule showing the Attribute held at the instance's
/// time (`observedAt`, else `modifiedAt`). A rule without a condition holds
/// at every time; a conditional one is read on the Entity's state at that
/// time ([`state_at`]) and does not hold where a member its condition reads
/// has no instance yet, or where it reads `scope`, which keeps no history:
/// a state that cannot be rebuilt hides the instance (fail closed). `None`
/// when no rule names one of the Entity's types.
pub(crate) fn rules_per_instance(doc: &Value, rules: &crate::policy::Filter) -> Option<Value> {
    let ctx = crate::repr::policy_context();
    let types = crate::policy::entity_types(doc);
    let typed: Vec<&crate::policy::Rule> = rules.rules.iter().filter(|r| r.selects(doc)).collect();
    if !rules.rules.is_empty() && typed.is_empty() {
        return None;
    }
    let omitted: std::collections::BTreeSet<&str> = types
        .iter()
        .filter_map(|t| rules.omit_by_type.get(*t))
        .flatten()
        .map(String::as_str)
        .collect();
    // a rule with windows decides per instance too (gap G6)
    let per_instance = |r: &crate::policy::Rule| r.is_conditional() || !r.temporal.is_empty();
    let always: std::collections::BTreeSet<&str> = typed
        .iter()
        .filter(|r| !per_instance(r))
        .flat_map(|r| r.attributes.iter().map(String::as_str))
        .collect();
    let conditional: Vec<(&crate::policy::Rule, Vec<String>)> = typed
        .iter()
        .filter(|r| per_instance(r) && r.scope_q.is_none())
        .map(|r| (*r, r.reads(ctx)))
        .collect();
    // an unconditioned rule over every Attribute keeps every instance
    let every = typed.iter().any(|r| !per_instance(r) && r.every_attribute);
    let now = chrono::Utc::now();
    let Some(obj) = doc.as_object() else {
        return Some(doc.clone());
    };
    let mut out = Map::new();
    for (k, v) in obj {
        if is_meta(k) {
            out.insert(k.clone(), v.clone());
            continue;
        }
        if omitted.contains(k.as_str()) {
            continue;
        }
        if rules.rules.is_empty() || every || always.contains(k.as_str()) {
            out.insert(k.clone(), v.clone());
            continue;
        }
        let deciding: Vec<&(&crate::policy::Rule, Vec<String>)> = conditional
            .iter()
            .filter(|(r, _)| r.every_attribute || r.attributes.iter().any(|a| a == k))
            .collect();
        let Some(instances) = v.as_array().filter(|_| !deciding.is_empty()) else {
            continue;
        };
        // ponytail: a condition rebuilds the state per instance, O(instances x
        // history); sort each Attribute's history once if long histories show
        // up under one. A rule without a condition reads no state: `typed`
        // already selected the Entity by its types and ids, which is all
        // `holds` reads of it, so only its windows decide (T-4246).
        let reads_state = deciding.iter().any(|(r, _)| r.is_conditional());
        let kept: Vec<Value> = instances
            .iter()
            .filter(|inst| {
                instant(inst).is_some_and(|t| {
                    let state = reads_state.then(|| state_at(doc, t));
                    let observed = inst.get("observedAt").and_then(Value::as_str);
                    deciding.iter().any(|(r, reads)| {
                        (!r.is_conditional()
                            || state.as_ref().is_some_and(|state| {
                                reads.iter().all(|a| state.get(a).is_some()) && r.holds(state, ctx)
                            }))
                            && r.temporal
                                .iter()
                                .all(|w| observed.is_some_and(|o| w.admits(o, now)))
                    })
                })
            })
            .cloned()
            .collect();
        if !kept.is_empty() {
            out.insert(k.clone(), Value::Array(kept));
        }
    }
    Some(Value::Object(out))
}

/// When an instance holds: its `observedAt`, else its `modifiedAt`.
pub(super) fn instant(inst: &Value) -> Option<&str> {
    inst.get("observedAt")
        .or_else(|| inst.get("modifiedAt"))
        .and_then(Value::as_str)
}

/// The Entity as it stood at `t`, rebuilt from its history: the frame, and
/// for each Attribute the latest instance at or before `t`, if any.
pub(super) fn state_at(doc: &Value, t: &str) -> Value {
    let at = dt_key(t);
    let Some(obj) = doc.as_object() else {
        return Value::Null;
    };
    let mut out = Map::new();
    for (k, v) in obj {
        if is_meta(k) {
            if k == "id" || k == "type" {
                out.insert(k.clone(), v.clone());
            }
            continue;
        }
        let latest = v
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|inst| instant(inst).map(|i| (dt_key(i), inst)))
            .filter(|(i, _)| *i <= at)
            .max_by(|a, b| a.0.cmp(&b.0))
            .map(|(_, inst)| inst.clone());
        if let Some(inst) = latest {
            out.insert(k.clone(), Value::Array(vec![inst]));
        }
    }
    Value::Object(out)
}

pub(super) fn window_conditions_match(
    doc: &Value,
    tq: Option<&TemporalQ>,
    q_ast: Option<&antares_ql::QNode>,
    geos: &[&antares_ql::geo::GeoQuery],
    ctx: &Context,
) -> bool {
    // 5.7.4.4 S2/S3: the values filter and geoquery are checked against
    // the Attribute instances WITHIN the temporal-query interval — an
    // out-of-window instance must not satisfy them
    let mut eval_doc = doc.clone();
    if let (Some(tqv), Some(o)) = (tq, eval_doc.as_object_mut()) {
        for (k, v) in o.iter_mut() {
            if is_meta(k) {
                continue;
            }
            if let Some(arr) = v.as_array_mut() {
                arr.retain(|inst| tqv.instance_matches(inst));
            }
        }
    }
    if let Some(ast) = q_ast {
        if !antares_ql::eval::eval_q(ast, &eval_doc, ctx, &|_| None) {
            return false;
        }
    }
    geos.iter().all(|g| g.matches(&eval_doc, ctx))
}

/// 5.7.4.4 S9: of each Attribute `q` reads, keep the instances that meet
/// the query: an instance meets it when `q` holds on the Entity with that
/// Attribute narrowed to the one instance (the other Attributes in the
/// window), so a term on another Attribute still acts at Entity level.
pub(super) fn retain_q_instances(
    doc: &mut Value,
    tq: Option<&TemporalQ>,
    ast: &antares_ql::QNode,
    ctx: &Context,
) {
    let mut heads: Vec<String> = ast
        .attribute_paths()
        .into_iter()
        .filter_map(|p| p.split(['.', '[', '{']).next().map(|h| ctx.expand_key(h)))
        .collect();
    heads.sort();
    heads.dedup();
    let mut eval_doc = doc.clone();
    if let (Some(tqv), Some(o)) = (tq, eval_doc.as_object_mut()) {
        for (k, v) in o.iter_mut() {
            if !is_meta(k) {
                if let Some(arr) = v.as_array_mut() {
                    arr.retain(|inst| tqv.instance_matches(inst));
                }
            }
        }
    }
    let (Some(obj), Value::Object(eval)) = (doc.as_object_mut(), &mut eval_doc) else {
        return;
    };
    for head in heads {
        let Some(arr) = obj.get_mut(&head).and_then(Value::as_array_mut) else {
            continue;
        };
        let windowed = eval.get(&head).cloned();
        arr.retain(|inst| {
            eval.insert(head.clone(), Value::Array(vec![inst.clone()]));
            // `eval` is lent back as a document for the one evaluation
            let probe = Value::Object(std::mem::take(eval));
            let holds = antares_ql::eval::eval_q(ast, &probe, ctx, &|_| None);
            if let Value::Object(m) = probe {
                *eval = m;
            }
            holds
        });
        if let Some(w) = windowed {
            eval.insert(head, w);
        }
    }
}

/// 5.7.4.4 S4 — and S7: the federation merge precedes this check, so
/// split and aggregated entities are re-filtered here too. The entity
/// qualifies if one of its scope's 4.18 validity intervals intersects
/// the query window; attribute instances outside every matching interval
/// are excluded (annex C.5.16).
pub(super) fn scope_window_retain(
    doc: &mut Value,
    sq: &str,
    tq: Option<&TemporalQ>,
    timeprop: &str,
) -> bool {
    let iv = scope_match_intervals(doc, sq);
    let (wstart, wend) = match tq {
        Some(t) => match t.timerel.as_str() {
            "before" => (None, Some(t.time_at.as_str())),
            "after" => (Some(t.time_at.as_str()), None),
            "between" => (Some(t.time_at.as_str()), t.end_time_at.as_deref()),
            _ => (None, None),
        },
        None => (None, None),
    };
    let intersects = iv.iter().any(|(s, e)| {
        wend.is_none_or(|w| s.as_str() < w)
            && e.as_deref().is_none_or(|e| wstart.is_none_or(|w| e > w))
    });
    if !intersects {
        return false;
    }
    if let Some(o) = doc.as_object_mut() {
        for (k, v) in o.iter_mut() {
            if is_meta(k) {
                continue;
            }
            if let Some(arr) = v.as_array_mut() {
                arr.retain(|inst| {
                    inst.get(timeprop).and_then(Value::as_str).is_some_and(|t| {
                        iv.iter()
                            .any(|(s, e)| t >= s.as_str() && e.as_deref().is_none_or(|e| t < e))
                    })
                });
            }
        }
    }
    true
}

pub(crate) async fn query_temporal_inner(
    st: &AppState,
    params: &HashMap<String, String>,
    headers: &HeaderMap,
    filter: &crate::policy::Filter,
) -> ApiResult<Response> {
    query_temporal_collected(st, params, headers, filter, None).await
}

/// The same query, with the merged documents of each page handed to `merged`
/// as well as rendered into the response. 5.16.1.4 fills a Snapshot from this
/// query and has to STORE what it matched: for an Evolution held by a
/// registered Context Source there is nothing else in this broker to store —
/// the fan-out's answer is the only copy, and the response body is a
/// presentation of it that compaction does not undo.
pub(crate) async fn query_temporal_collected(
    st: &AppState,
    params: &HashMap<String, String>,
    headers: &HeaderMap,
    filter: &crate::policy::Filter,
    merged: Option<&mut Vec<Value>>,
) -> ApiResult<Response> {
    let tenant = tenant_from(headers)?;
    check_params(
        params,
        &[
            "id",
            "idPattern",
            "type",
            "attrs",
            "q",
            "georel",
            "geometry",
            "coordinates",
            "geoproperty",
            "scopeQ",
            "csf",
            "timerel",
            "timeAt",
            "endTimeAt",
            "timeproperty",
            "aggrMethods",
            "aggrPeriodDuration",
            "lastN",
            "firstN",
            "offsetN",
            "limit",
            "offset",
            "count",
            "options",
            "format",
            "lang",
            "local",
            "entityMap",
            "pick",
            "omit",
            "datasetId",
            "orderBy",
            "orderFrom",
            "orderGeometry",
            "collation",
            "entityMapLifetime",
            "splitEntities",
            "expandValues",
            "jsonKeys",
        ],
    )?;
    let accept = parse_accept(headers)?;
    let ctx = request_context(&st.loader, headers).await?;
    // 5.7.4.4 a-e: id/idPattern alone are NOT sufficient, and the attrs
    // list / q must include at least one non-system Attribute to qualify.
    // 5.7.4.3 expandValues: the same 4.9 EXAMPLE 12 coercion as the entity
    // query — term values expanded against the @context before executing,
    // less the Attributes jsonKeys declares uninterpretable.
    let q_ast = params.get("q").map(|q| parse_q(q)).transpose()?.map(|ast| {
        antares_ql::eval::apply_expand_values(
            ast,
            antares_ql::eval::expansion_list(
                params.get("expandValues").map(String::as_str),
                params.get("jsonKeys").map(String::as_str),
            )
            .as_deref(),
            &ctx,
        )
    });
    check_temporal_query(st, params, headers, &tenant, &ctx, q_ast.as_ref()).await?;
    // ADR-0020: the engine's narrowing joins the request's own filters after
    // 5.7.4.4's "too wide" judgement, which is about what the client asked
    // for. Everything below reads the narrowed query, the forward included.
    // ADR-0023 rule 11: the rules, and the client's own q apart from the
    // engine's, read on each Entity as its rules show it
    let types_rule = crate::repr::type_rules(filter);
    let ruled = types_rule.is_some();
    let client_q = if ruled {
        crate::repr::client_q(params, &ctx)?
    } else {
        None
    };
    let narrowed = filter.narrow_window(&filter.narrow_params(params, &ctx)?, true)?;
    let params = &narrowed;
    let q_ast = params
        .get("q")
        .map(|q| antares_ql::parse_narrowed_q(q))
        .transpose()?
        .map(|ast| {
            antares_ql::eval::apply_expand_values(
                ast,
                antares_ql::eval::expansion_list(
                    params.get("expandValues").map(String::as_str),
                    params.get("jsonKeys").map(String::as_str),
                )
                .as_deref(),
                &ctx,
            )
        });
    // under rules only the engine's own q reaches the store and the stored
    // history; the client's waits for the shown one
    let q_ast = if ruled {
        filter.q.as_deref().cloned()
    } else {
        q_ast
    };
    let scope_q = params.get("scopeQ").map(String::as_str);
    let tq = TemporalQ::from_params(params, true)?;
    let mut trepr = parse_trepr(params, &ctx)?;
    crate::repr::narrow_projection(&mut trepr.pick, &mut trepr.omit, filter, &ctx)?;
    // 5.7.4.4: {…} projection is Linked Entity retrieval — unconditional 400
    reject_linked_projection(&trepr, "5.7.4.4")?;
    let page_n = trepr.page;

    let ids: Option<Vec<&str>> = params.get("id").map(|s| s.split(',').collect());
    // 5.7.4.4: an invalid URI in the id list is BadRequestData
    if let Some(ids) = &ids {
        for id in ids {
            antares_model::EntityId::new(id)?;
        }
    }
    let id_pattern = match params.get("idPattern") {
        Some(p) => Some(
            antares_ql::regex::compile(p)
                .map_err(|_| NgsiError::BadRequestData(format!("invalid idPattern {p:?}")))?,
        ),
        None => None,
    };
    let types: Option<Vec<String>> = params.get("type").map(|s| {
        s.split([',', '|'])
            .map(|t| ctx.expand_key(t.trim()))
            .collect()
    });
    let attrs_filter = selection(&trepr);
    // only the attrs= param excludes entities; pick is projection-only
    let (entity_attr_filter, shown_attrs) = if ruled {
        (None, trepr.attrs.clone())
    } else {
        (trepr.attrs.clone(), None)
    };
    // the request's own geoquery and the narrowing's area, all of which
    // must hold (ADR-0023 rule 1); under rules the request's reads the
    // shown history
    let own_geo = antares_ql::geo::GeoQuery::from_params(params)?;
    let (geos, shown_geos): (
        Vec<&antares_ql::geo::GeoQuery>,
        Vec<&antares_ql::geo::GeoQuery>,
    ) = if ruled {
        (filter.areas().collect(), own_geo.iter().collect())
    } else {
        (own_geo.iter().chain(filter.areas()).collect(), Vec::new())
    };

    // Push entity narrowing (ids/types/attrs) and instance-window
    // pruning (range + RANK()-capped lastN) into the store. The loop below
    // and window() stay the arbiters — pruning is byte-exact against
    // instance_matches (compile::temporal), so it cannot change an answer.
    // 5.7.4.4 S2/S3: q and geo are judged on the instances WITHIN the
    // temporal interval (the eval_doc retain below), so RANGE pruning is
    // verdict-safe to push even with q=/geo present. Only the lastN cap
    // (its ordering vs the values filter is unspecified) and entity paging
    // (q/geo still drop entities after SQL) wait for exactness.
    // scopeQ joins q/geo here: the 4.18 validity filter drops entities and
    // instances AFTER SQL, so a pushed page/lastN cap would under-return.
    let exact_push = q_ast.is_none() && geos.is_empty() && scope_q.is_none();
    // Entity-page pushdown: a temporal query used to materialize the
    // tenant's ENTIRE history. Pushed only when every filter the store
    // cannot see is absent — same gate family as the entity-query pushdown.
    // A values filter no longer blocks paging when its prefilter compiles
    // EXACTLY (every leaf a Cmp with the byte-exact text window): the SQL
    // entity verdict then equals the evaluator's. datasetId/pick still
    // block: their entity drops happen at presentation, after the page.
    let q_page_exact = q_ast.as_ref().is_none_or(|ast| {
        let r = tq.as_ref().map(|t| antares_store::filter::InstanceRange {
            timerel: &t.timerel,
            time_at: &t.time_at,
            end_time_at: t.end_time_at.as_deref(),
            timeproperty: &t.timeproperty,
        });
        st.temporal
            .q_pushdown_exact(ast, r.as_ref(), &|t| ctx.expand_key(t))
    });
    let (p_offset, p_limit, _) = crate::paging::page_params(st, params)?;
    // rule 8 drops Entities after the store answered, so the store may not
    // page under a narrowing that reads the state now (`ponytail:` one read
    // per Entity; push it into the store query when it shows up)
    let narrowed_now =
        filter.q.is_some() || filter.scope_q.is_some() || filter.areas().next().is_some();
    // 5.7.4.4 + 5.5.9: pagination applies to the MERGED federated union, so
    // the store may only pre-page when nothing will federate — otherwise
    // page 1 is local-page + every remote row (matrix-9 IOP_EXT_TMP_03_04).
    let push_page = (exact_push || (geos.is_empty() && scope_q.is_none() && q_page_exact))
        && !ruled
        && id_pattern.is_none()
        && params.get("orderBy").is_none()
        && params.get("datasetId").is_none()
        && params.get("pick").is_none()
        && p_limit > 0
        && !narrowed_now
        && !crate::federation::would_federate(st, &tenant, &ctx, params, headers).await?;
    // 4.5.19 computed by the store: the numeric bucket matrix per attribute
    // comes back aggregated when nothing after the store call could change
    // the answer — every filter exact in SQL, the page pushed, no
    // projection/lastN/month periods — otherwise the instances are
    // aggregated here as before. A store that cannot (memory) or a
    // non-numeric value class leaves `outcome.aggregated` false.
    let push_agg = trepr.aggregated
        && !crate::history::records_on_change(st, &tenant)
        && exact_push
        && push_page
        && trepr.omit.is_none()
        && page_n == InstancePage::default()
        && !matches!(trepr.aggr_period, AggrPeriod::Months(..))
        && params.get("entityMap").map(String::as_str) != Some("true")
        && trepr
            .aggr_methods
            .iter()
            .all(|m| antares_store::filter::AGGREGATE_METHODS.contains(&m.as_str()));
    // scoped: the &dyn expander must not live across an await (handler
    // futures are Send; the store call itself is synchronous)
    let outcome = {
        let expand = |t: &str| ctx.expand_key(t);
        let geo_pre: Vec<_> = geos.iter().map(|g| g.to_instance_spec(&ctx)).collect();
        let tf = antares_store::filter::TemporalFilter {
            ids: ids.as_deref(),
            types: types.as_deref(),
            attrs: entity_attr_filter.as_deref(),
            // a condition rebuilds each instance's state from the whole
            // history; rules without one read only the window (T-4246)
            range: tq
                .as_ref()
                .filter(|_| !replays_state(types_rule.as_deref()))
                .map(|t| antares_store::filter::InstanceRange {
                    timerel: &t.timerel,
                    time_at: &t.time_at,
                    end_time_at: t.end_time_at.as_deref(),
                    timeproperty: &t.timeproperty,
                }),
            last_n: page_n.store_cap().filter(|_| {
                exact_push
                    && cap_keeps_shown(
                        types_rule.as_deref(),
                        &page_n,
                        tq.as_ref()
                            .map_or("observedAt", |t| t.timeproperty.as_str()),
                    )
            }),
            oldest_first: !page_n.desc,
            timeproperty: tq
                .as_ref()
                .map_or("observedAt", |t| t.timeproperty.as_str()),
            page: push_page.then_some(antares_store::filter::Page {
                offset: p_offset as i64,
                limit: p_limit as i64,
                count: true,
            }),
            q: q_ast.as_ref(),
            expand: &expand,
            geo: &geo_pre,
            aggregate: push_agg.then_some(antares_store::filter::Aggregate {
                methods: &trepr.aggr_methods,
                period_secs: match trepr.aggr_period {
                    AggrPeriod::Seconds(sc) => Some(sc),
                    _ => None,
                },
                anchor: tq.as_ref().map(|t| t.time_at.as_str()),
            }),
        };
        st.temporal.query_temporal(&tenant, &tf).await?
    };
    if outcome.aggregated {
        return respond_aggregated(
            st,
            params,
            &tenant,
            &ctx,
            accept,
            &trepr,
            tq.as_ref(),
            attrs_filter.as_ref(),
            outcome,
        );
    }
    let (all, pre_paged, pre_total) = (outcome.rows, outcome.paged, outcome.total);
    // 5.7.4.4: fan the query out to matching queryTemporal registrations
    // and merge the remote Temporal Evolutions with the local set (4.5.5;
    // auxiliary data never introduces new entities)
    let mut warnings: Vec<String> = Vec::new();
    let looped = crate::federation::via_loop(
        headers,
        &crate::federation::alias_for(&st.host_alias, &tenant),
    );
    let timeprop = tq
        .as_ref()
        .map_or("observedAt", |t| t.timeproperty.as_str())
        .to_owned();
    let all = if crate::federation::active(params) && !looped {
        let fed = crate::federation::fed_query_temporal(
            st,
            &tenant,
            headers,
            &ctx,
            params,
            &mut warnings,
        )
        .await?;
        let mut order: Vec<String> = Vec::new();
        let mut by_id: std::collections::HashMap<String, Value> = Default::default();
        for doc in all {
            if let Some(id) = doc.get("id").and_then(Value::as_str) {
                order.push(id.to_owned());
                by_id.insert(id.to_owned(), doc);
            }
        }
        for aux_pass in [false, true] {
            for (aux, d) in &fed {
                if *aux != aux_pass {
                    continue;
                }
                let Some(id) = d.get("id").and_then(Value::as_str) else {
                    continue;
                };
                match by_id.get_mut(id) {
                    Some(base) => merge_temporal_docs(base, d, *aux, &timeprop),
                    None if !aux => {
                        order.push(id.to_owned());
                        by_id.insert(id.to_owned(), d.clone());
                    }
                    None => {}
                }
            }
        }
        order
            .into_iter()
            .filter_map(|id| by_id.remove(&id))
            .collect()
    } else {
        all
    };
    let mut matches = Vec::new();
    for mut doc in all {
        let id = doc.get("id").and_then(Value::as_str).unwrap_or("");
        if let Some(ids) = &ids {
            if !ids.contains(&id) {
                continue;
            }
        }
        // 5.2.33: "id takes precedence over idPattern"
        if ids.is_none() {
            if let Some(re) = &id_pattern {
                if !re.is_match(id) {
                    continue;
                }
            }
        }
        if !type_and_attrs_match(&doc, types.as_ref(), entity_attr_filter.as_ref()) {
            continue;
        }
        if (q_ast.is_some() || !geos.is_empty())
            && !window_conditions_match(&doc, tq.as_ref(), q_ast.as_ref(), &geos, &ctx)
        {
            continue;
        }
        if let Some(ast) = &q_ast {
            retain_q_instances(&mut doc, tq.as_ref(), ast, &ctx);
        }
        if let Some(sq) = scope_q {
            if !scope_window_retain(&mut doc, sq, tq.as_ref(), timeprop.as_str()) {
                continue;
            }
        }
        if let Some(rules) = types_rule.as_deref() {
            let Some(shown) = rules_per_instance(&doc, rules) else {
                continue;
            };
            doc = shown;
            if !type_and_attrs_match(&doc, None, shown_attrs.as_ref()) {
                continue;
            }
            if (client_q.is_some() || !shown_geos.is_empty())
                && !window_conditions_match(&doc, tq.as_ref(), client_q.as_ref(), &shown_geos, &ctx)
            {
                continue;
            }
            if let Some(ast) = &client_q {
                retain_q_instances(&mut doc, tq.as_ref(), ast, &ctx);
            }
        }
        // entity qualifies only if some instance falls in the window
        let any_instance = doc.as_object().is_some_and(|o| {
            o.iter().any(|(k, v)| {
                !is_meta(k)
                    && v.as_array().is_some_and(|arr| {
                        arr.iter()
                            .any(|inst| tq.as_ref().is_none_or(|tq| tq.instance_matches(inst)))
                    })
            })
        });
        if !any_instance {
            continue;
        }
        if narrowed_now {
            let id = doc
                .get("id")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_owned();
            if !member_now(st, &tenant, &ctx, filter, &id, None).await? {
                continue;
            }
        }
        matches.push(doc);
    }
    if let Some(spec) = params.get("orderBy") {
        crate::paging::order_entities(&mut matches, spec, params, &ctx)?;
    }
    let (page, count_hdr, links) = if pre_paged {
        let total = pre_total.map(|t| t as usize).unwrap_or(matches.len());
        crate::paging::paginate_pre(st, params, matches, "/ngsi-ld/v1/temporal/entities", total)?
    } else {
        crate::paging::paginate(st, params, matches, "/ngsi-ld/v1/temporal/entities")?
    };
    if let Some(sink) = merged {
        sink.extend(page.iter().cloned());
    }
    let core_only_pick = attrs_filter.as_ref().is_some_and(Vec::is_empty);
    let mut payload: Vec<Value> = Vec::new();
    let (mut g_trunc, mut g_min, mut g_maxts) = (false, None::<String>, None::<String>);
    let mut g_more = false;
    // the next page's move: the fewest instances any Entity's cut Attribute
    // served, and the boundary every Entity's cut reached (`interval_links`)
    let (mut g_cut, mut g_bound) = (None::<usize>, None::<String>);
    // ADR-0023 rule 11: each Entity's history was narrowed by its rules in
    // the loop above, before the window, so lastN counts only what may be
    // seen (rule 8)
    for d in &page {
        let mut w = window(
            d,
            tq.as_ref(),
            page_n,
            attrs_filter.as_ref(),
            trepr.omit.as_ref(),
            trepr.dataset_id.as_ref(),
            &timeprop,
            crate::history::held_attrs(st, &tenant, d),
        );
        if !trepr.aggregated {
            truncate(
                &mut w,
                &timeprop,
                page_n.desc,
                page_n.size(st.temporal_default_instances, st.temporal_instance_limit),
            );
        }
        g_trunc |= w.truncated;
        g_more |= w.more;
        if let Some(c) = w.cut_served {
            g_cut = Some(g_cut.map_or(c, |g| g.min(c)));
        }
        if let Some(b) = w.truncated.then(|| cut_boundary(&w, page_n.desc)).flatten() {
            // the earliest boundary forwards, the latest backwards: no
            // Entity's instance past its own boundary is left behind
            if g_bound
                .as_deref()
                .is_none_or(|g| (dt_key(b) < dt_key(g)) != page_n.desc)
            {
                g_bound = Some(b.to_owned());
            }
        }
        if let Some(m) = &w.ts_min {
            if g_min.as_deref().is_none_or(|c| dt_key(m) < dt_key(c)) {
                g_min = Some(m.clone());
            }
        }
        if let Some(m) = &w.ts_max {
            if g_maxts.as_deref().is_none_or(|c| dt_key(m) > dt_key(c)) {
                g_maxts = Some(m.clone());
            }
        }
        // no instance survived the window/dataset filters ⇒ the entity is
        // not part of the temporal result (unless the projection is
        // deliberately core-only, e.g. pick=id)
        if w.attrs.is_empty() && !core_only_pick {
            continue;
        }
        let presented = present_temporal(d, &w, &ctx, &trepr, tq.as_ref(), &timeprop)?;
        if trepr.pick.is_some() && presented.as_object().is_some_and(|o| o.is_empty()) {
            continue;
        }
        payload.push(presented);
    }
    // aggregated responses are complete by construction — never 206 (6.3.10)
    let cr = if trepr.aggregated {
        None
    } else {
        content_range(
            g_trunc,
            g_min.as_deref(),
            g_maxts.as_deref(),
            tq.as_ref(),
            page_n,
        )
    };
    let status = if cr.is_some() {
        StatusCode::PARTIAL_CONTENT
    } else {
        StatusCode::OK
    };
    let mut resp = crate::negotiate::respond_list(status, payload, &ctx, accept, &tenant);
    crate::paging::attach_warnings(&mut resp, &warnings);
    if let Some(cr) = cr {
        if let Ok(v) = cr.parse() {
            resp.headers_mut().insert("Content-Range", v);
        }
    }
    attach_paging(&mut resp, count_hdr, &links);
    attach_paging(
        &mut resp,
        None,
        &interval_links(
            params,
            "/ngsi-ld/v1/temporal/entities",
            accept,
            page_n,
            g_more,
            page_n.step(
                trepr.aggregated,
                st.temporal_default_instances,
                st.temporal_instance_limit,
            ),
            g_cut.zip(g_bound.as_deref()),
        ),
    );
    // 6.18.3.2: entityMap=true — the temporal EntityMap for this query is
    // (re)created; the response carries NGSILD-EntityMap and 201 Created.
    if params.get("entityMap").map(String::as_str) == Some("true") {
        let map = build_temporal_map(st, &tenant, headers, &ctx, params, filter).await?;
        *resp.status_mut() = StatusCode::CREATED;
        if let Some(id) = map.get("id").and_then(Value::as_str) {
            if let Ok(v) = format!("/ngsi-ld/v1/entityMaps/{id}").parse() {
                resp.headers_mut().insert("NGSILD-EntityMap", v);
            }
        }
    }
    filter.mark(&st.host_alias, resp.headers_mut());
    Ok(resp)
}

// ---------- POST /temporal/entityOperations/query (6.24) ----------

pub async fn batch_temporal_query(
    State(st): State<AppState>,
    CleanParams(params): CleanParams,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let go = async {
        // 6.3.14 and 6.3.4: a Tenant outside the grammar and an Accept the
        // operation cannot serve are both refused here. Neither VALUE is
        // needed — the inner query reads the headers again — but the request
        // must not reach it having skipped either check.
        let tenant = tenant_from(&headers)?;
        check_params(
            &params,
            &["limit", "offset", "count", "options", "format", "local"],
        )?;
        parse_accept(&headers)?;
        let parsed = parse_body(&st, &headers, &body, BodyKind::Standard).await?;
        let q = parsed.object(NgsiError::BadRequestData(
            "query body must be an object".into(),
        ))?;
        if q.get("type").and_then(Value::as_str) != Some("Query") {
            return Err(NgsiError::BadRequestData("body type must be Query".into()).into());
        }
        // 5.2.23 Query (temporal reading): members flattened with their
        // Table 5.2.23-1 value spaces enforced, incl. temporalQ (5.2.21)
        // and aggrParams (5.2.44).
        let mut vp: HashMap<String, String> = params.clone();
        crate::paging::query_doc_params(q, true, &mut vp)?;
        // ADR-0020: the engine is shown every member of the Query body
        let named = crate::entities::ReadMembers::with_ctx(&st, &vp, &parsed.ctx);
        let filter = gate!(
            st, &tenant, &headers, "5.7.4",
            q: named.q.as_ref(),
            scope_q: vp.get("scopeQ").map(String::as_str),
            types: &named.types,
            attrs: &named.attrs,
            geo: named.geo.as_ref(),
        )
        .await?;
        query_temporal_inner(&st, &vp, &headers, &filter).await
    };
    go.await.unwrap_or_else(|e| e.into_response())
}

/// 5.14.5.4: temporal query required; the S1–S4 candidate selection is the
/// temporal query pipeline itself (5.7.4.4) run unpaged — the ids of its
/// result set form the EntityMap; the createEntityMapQueryTemporal
/// registrations are then merged like 5.14.4.
/// Known ceiling: candidate ids are read from the internal 5.7.4 response capped
/// at max_limit; raise the cap if temporal sets outgrow it.
pub(crate) async fn build_temporal_map(
    st: &AppState,
    tenant: &TenantId,
    headers: &HeaderMap,
    ctx: &antares_jsonld::Context,
    params: &HashMap<String, String>,
    filter: &crate::policy::Filter,
) -> ApiResult<Value> {
    if !params.contains_key("timerel") {
        return Err(NgsiError::BadRequestData(
            "a temporal query is required to create a temporal EntityMap (5.14.5.4)".into(),
        )
        .into());
    }
    let local_scope = params.get("local").map(String::as_str) == Some("true");
    let split = params.get("splitEntities").map(String::as_str) == Some("true");
    let mut eff: HashMap<String, String> = if split && !local_scope {
        params
            .iter()
            .filter(|(k, _)| {
                [
                    "id",
                    "idPattern",
                    "type",
                    "local",
                    "timerel",
                    "timeAt",
                    "endTimeAt",
                    "timeproperty",
                ]
                .contains(&k.as_str())
            })
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect()
    } else {
        params.clone()
    };
    for k in [
        "entityMap",
        "entityMapLifetime",
        "splitEntities",
        "offset",
        "count",
    ] {
        eff.remove(k);
    }
    eff.insert("limit".into(), st.max_limit.to_string());
    // Box::pin: build_temporal_map is reachable from query_temporal_inner
    // (entityMap=true), so this recursive edge needs indirection.
    let resp = Box::pin(query_temporal_inner(st, &eff, headers, filter)).await?;
    let bytes = axum::body::to_bytes(resp.into_body(), st.max_answer_bytes)
        .await
        .map_err(|_| NgsiError::TooManyResults(crate::bounds::ANSWER_TOO_LARGE.into()))?;
    // 5.5.14: the map FIXES the Entities considered by every later request, so
    // an unreadable candidate set must fail rather than become "no candidates".
    let candidates: Value = serde_json::from_slice(&bytes)
        .map_err(|_| NgsiError::InternalError("temporal candidate parse".into()))?;
    let mut emap = Map::new();
    if let Some(arr) = candidates.as_array() {
        for d in arr {
            if let Some(id) = d.get("id").and_then(Value::as_str) {
                emap.insert(id.to_owned(), json!(["@none"]));
            }
        }
    }
    crate::entity_map::merge_and_store_map(st, tenant, headers, ctx, params, true, emap).await
}
