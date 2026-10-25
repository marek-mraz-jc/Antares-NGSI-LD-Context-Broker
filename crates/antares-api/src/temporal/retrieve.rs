// SPDX-License-Identifier: EUPL-1.2
//! Retrieve temporal evolution of one Entity (5.7.3).

use super::*;

// ---------- GET /temporal/entities/{id} (5.7.3) ----------

pub async fn retrieve_temporal(
    State(st): State<AppState>,
    Path(id): Path<String>,
    CleanParams(params): CleanParams,
    headers: HeaderMap,
) -> Response {
    match retrieve_temporal_outer(&st, &id, &params, &headers).await {
        Ok(r) => r,
        Err(e) => e.into_response(),
    }
}

/// 5.7.3.4 Retrieve Temporal Evolution: the EntityMap half of the clause is
/// the shared rule (`entity_maps::retrieve_with_map`); this is the retrieve
/// it wraps.
pub(super) async fn retrieve_temporal_outer(
    st: &AppState,
    id: &str,
    params: &HashMap<String, String>,
    headers: &HeaderMap,
) -> ApiResult<Response> {
    crate::entity_map::retrieve_with_map(st, id, params, headers, true, |map| async move {
        retrieve_temporal_inner(st, id, params, headers, map.as_ref()).await
    })
    .await
}

/// T-4665, ADR-N-054 2.4: `versions=all`, the chain of each measurement, for
/// a caller the space's Policy granted `readVersions` (the gateway says so).
/// A platform extension (feature `jc`, T-4854).
#[cfg(feature = "jc")]
pub(super) fn versions_asked(
    st: &AppState,
    params: &HashMap<String, String>,
    headers: &HeaderMap,
) -> ApiResult<bool> {
    let versions = match params.get("versions").map(String::as_str) {
        None => false,
        Some("all") => true,
        Some(other) => {
            return Err(NgsiError::BadRequestData(format!(
                "versions takes only `all`, not {other:?}"
            ))
            .into())
        }
    };
    if versions && !crate::provenance::may_read_versions(headers, st.provenance_from.as_deref()) {
        return Err(ApiError::Denied(
            "versions=all needs readVersions, which the space's Policy grants through the gateway"
                .into(),
        ));
    }
    Ok(versions)
}

/// Vanilla: `check_params` already refused `versions` as unknown.
#[cfg(not(feature = "jc"))]
pub(super) fn versions_asked(
    _st: &AppState,
    _params: &HashMap<String, String>,
    _headers: &HeaderMap,
) -> ApiResult<bool> {
    Ok(false)
}

pub(super) async fn retrieve_temporal_inner(
    st: &AppState,
    id: &str,
    params: &HashMap<String, String>,
    headers: &HeaderMap,
    map: Option<&Value>,
) -> ApiResult<Response> {
    let tenant = tenant_from(headers)?;
    check_params(params, RETRIEVE_PARAMS)?;
    let versions = versions_asked(st, params, headers)?;
    let accept = parse_accept(headers)?;
    let ctx = request_context(&st.loader, headers).await?;
    let filter = gate!(st, &tenant, headers, "5.7.3", ids: &[id]).await?;
    // gap G6: every reader below takes the window clamped to the grant
    let windowed = filter.narrow_window(params, false)?;
    let params = &windowed;
    let tq = TemporalQ::from_params(params, false)?;
    let mut trepr = parse_trepr(params, &ctx)?;
    crate::repr::narrow_projection(&mut trepr.pick, &mut trepr.omit, &filter, &ctx)?;
    // 5.7.3.4: "If projection attributes are present and indicate the
    // use of Linked Entity retrieval, an error of type BadRequestData
    // shall be raised" — unconditional, temporal defines no join.
    reject_linked_projection(&trepr, "5.7.3.4")?;
    antares_model::EntityId::new(id)?;
    let types_rule = crate::repr::type_rules(&filter);
    let timeprop = tq
        .as_ref()
        .map_or("observedAt", |t| t.timeproperty.as_str())
        .to_owned();
    let req = RetrieveReq {
        st,
        tenant: &tenant,
        headers,
        ctx: &ctx,
        id,
        params,
        map,
    };
    let tf = retrieve_filter(tq.as_ref(), trepr.page, types_rule.as_deref(), &timeprop);
    let mut warnings: Vec<String> = Vec::new();
    let doc = load_evolution(&req, &tf, &timeprop, &mut warnings).await?;
    let not_found = || NgsiError::ResourceNotFound(format!("temporal entity {id} not found"));
    // ADR-0020, ADR-0023 rules 3 and 8: an Entity outside the
    // narrowing now is the 404 of an absent one
    if !member_now(st, &tenant, &ctx, &filter, id, Some(&doc)).await? {
        return Err(not_found().into());
    }
    // ADR-0023 rule 11: the history its rules show, before anything
    // reads it; no rule for its types is the 404 of an absent Entity
    let doc = match types_rule.as_deref() {
        Some(r) => rules_per_instance(&doc, r).ok_or_else(not_found)?,
        None => doc,
    };
    let attrs_filter = selection(&trepr);
    check_selection(&doc, &trepr, attrs_filter.as_ref(), id)?;
    let mut w = window(
        &doc,
        tq.as_ref(),
        trepr.page,
        attrs_filter.as_ref(),
        trepr.omit.as_ref(),
        trepr.dataset_id.as_ref(),
        &timeprop,
        crate::history::held_attrs(st, &tenant, &doc),
    );
    if !trepr.aggregated {
        let page_n = trepr.page;
        truncate(
            &mut w,
            &timeprop,
            page_n.desc,
            page_n.size(st.temporal_default_instances, st.temporal_instance_limit),
        );
        // after the cut: lastN counts measurements, never versions (DM-119)
        if versions {
            with_versions(&mut w, &st.temporal.versions(&tenant, id).await?);
        }
    }
    let payload = present_temporal(&doc, &w, &ctx, &trepr, tq.as_ref(), &timeprop)?;
    if (trepr.pick.is_some() || trepr.omit.is_some())
        && payload.as_object().is_some_and(|o| o.is_empty())
    {
        return Err(NgsiError::ResourceNotFound(format!(
            "projection matches nothing on temporal entity {id}"
        ))
        .into());
    }
    let mut resp = respond_retrieved(&req, accept, &w, &trepr, tq.as_ref(), payload);
    crate::paging::attach_warnings(&mut resp, &warnings);
    filter.mark(&st.host_alias, resp.headers_mut());
    Ok(resp)
}

/// The parameters 5.7.3 accepts (6.19.3.1 Table 6.19.3.1-1).
const RETRIEVE_PARAMS: &[&str] = &[
    "attrs",
    "timerel",
    "timeAt",
    "endTimeAt",
    "timeproperty",
    "lastN",
    "firstN",
    "offsetN",
    "aggrMethods",
    "aggrPeriodDuration",
    "options",
    "format",
    "lang",
    "local",
    "pick",
    "omit",
    "datasetId",
    "entityMap",
    "entityMapLifetime",
    // the platform's extension (T-4854): vanilla refuses it as unknown
    #[cfg(feature = "jc")]
    "versions",
];

/// One 5.7.3 request as the stages after the gate read it; `params` is
/// the window clamped to the grant.
struct RetrieveReq<'a> {
    st: &'a AppState,
    tenant: &'a TenantId,
    headers: &'a HeaderMap,
    ctx: &'a Context,
    id: &'a str,
    params: &'a HashMap<String, String>,
    map: Option<&'a Value>,
}

/// Instance pruning pushed into the store (no q=/geo on retrieve, so it
/// is always safe here); window() stays the arbiter. ADR-0023 rule 11: a
/// condition rebuilds each instance's state from the whole history, so
/// the store prunes no instance under one; rules without one read only
/// the window (T-4246).
fn retrieve_filter<'a>(
    tq: Option<&'a TemporalQ>,
    page_n: InstancePage,
    types_rule: Option<&crate::policy::Filter>,
    timeprop: &'a str,
) -> antares_store::filter::TemporalFilter<'a> {
    let replays = replays_state(types_rule);
    antares_store::filter::TemporalFilter {
        range: tq
            .filter(|_| !replays)
            .map(|t| antares_store::filter::InstanceRange {
                timerel: &t.timerel,
                time_at: &t.time_at,
                end_time_at: t.end_time_at.as_deref(),
                timeproperty: &t.timeproperty,
            }),
        last_n: page_n
            .store_cap()
            .filter(|_| cap_keeps_shown(types_rule, &page_n, timeprop)),
        oldest_first: !page_n.desc,
        timeproperty: timeprop,
        ..Default::default()
    }
}

/// The Entity's Temporal Evolution: the local one, and under 5.7.3.4 the
/// matching retrieveTemporal registrations' merged into it (4.5.5;
/// auxiliary instances only fill timestamps absent elsewhere).
async fn load_evolution(
    req: &RetrieveReq<'_>,
    tf: &antares_store::filter::TemporalFilter<'_>,
    timeprop: &str,
    warnings: &mut Vec<String>,
) -> ApiResult<Value> {
    let (st, tenant, id) = (req.st, req.tenant, req.id);
    let local = match st.temporal.get_temporal(tenant, id, tf).await? {
        Some(doc) => Some(doc),
        None => kept_no_history(st, tenant, id).await?,
    };
    let looped = crate::federation::via_loop(
        req.headers,
        &crate::federation::alias_for(&st.host_alias, tenant),
    );
    if !crate::federation::active(req.params) || looped {
        return Ok(local.ok_or_else(|| {
            NgsiError::ResourceNotFound(format!("temporal entity {id} not found"))
        })?);
    }
    let fed = crate::federation::fed_retrieve_temporal(
        st,
        tenant,
        req.headers,
        req.ctx,
        id,
        req.params,
        req.map,
        warnings,
    )
    .await?;
    Ok(merge_retrieved(local, &fed, id, timeprop)?)
}

/// The local Evolution, or the first exclusive remote one when there is
/// none (any remote one when all are auxiliary), with every other remote
/// copy merged in, the exclusive ones first.
fn merge_retrieved(
    local: Option<Value>,
    fed: &[(bool, Value)],
    id: &str,
    timeprop: &str,
) -> Result<Value, NgsiError> {
    let (mut base, skip) = match local {
        Some(b) => (b, None),
        None => {
            let idx = fed.iter().position(|(aux, _)| !aux).or(if fed.is_empty() {
                None
            } else {
                Some(0)
            });
            match idx.and_then(|i| fed.get(i).map(|(_, d)| (d.clone(), i))) {
                Some((d, i)) => (d, Some(i)),
                None => {
                    return Err(NgsiError::ResourceNotFound(format!(
                        "temporal entity {id} not found"
                    )))
                }
            }
        }
    };
    for aux_pass in [false, true] {
        for (i, (aux, d)) in fed.iter().enumerate() {
            if Some(i) == skip {
                continue;
            }
            if *aux == aux_pass {
                merge_temporal_docs(&mut base, d, *aux, timeprop);
            }
        }
    }
    Ok(base)
}

/// 5.7.3: attrs matching nothing ⇒ 404. A `pick` may name core members
/// instead — 5.7.3.3 admits "id", "type", "scope" or an Attribute name
/// — and then it selects no Attribute at all. 5.7.3.5 still reduces the
/// Entity to the members it names, so that empty selection is the
/// answer rather than a Temporal Evolution holding none of it.
fn check_selection(
    doc: &Value,
    trepr: &TRepr,
    attrs_filter: Option<&Vec<String>>,
    id: &str,
) -> Result<(), NgsiError> {
    let core_only_pick =
        trepr.attrs.is_none() && trepr.pick.is_some() && attrs_filter.is_some_and(Vec::is_empty);
    match attrs_filter {
        Some(want) if !core_only_pick && !want.iter().any(|a| doc.get(a).is_some()) => {
            Err(NgsiError::ResourceNotFound(format!(
                "temporal entity {id} has none of the requested attributes"
            )))
        }
        _ => Ok(()),
    }
}

/// The Evolution's body, 200 or 206 with its Content-Range, and the
/// 6.4.7.3 links to the next page.
fn respond_retrieved(
    req: &RetrieveReq<'_>,
    accept: Accept,
    w: &Windowed,
    trepr: &TRepr,
    tq: Option<&TemporalQ>,
    payload: Value,
) -> Response {
    let (st, page_n) = (req.st, trepr.page);
    // aggregated responses are complete by construction — never 206 (6.3.10)
    let cr = if trepr.aggregated {
        None
    } else {
        content_range(
            w.truncated,
            w.ts_min.as_deref(),
            w.ts_max.as_deref(),
            tq,
            page_n,
        )
    };
    let status = if cr.is_some() {
        StatusCode::PARTIAL_CONTENT
    } else {
        StatusCode::OK
    };
    let mut resp = respond(status, payload, req.ctx, accept, req.tenant);
    attach_paging(
        &mut resp,
        None,
        &interval_links(
            req.params,
            &format!(
                "/ngsi-ld/v1/temporal/entities/{}",
                crate::federation::path_segment(req.id)
            ),
            accept,
            page_n,
            w.more,
            page_n.step(
                trepr.aggregated,
                st.temporal_default_instances,
                st.temporal_instance_limit,
            ),
            w.cut_served.zip(cut_boundary(w, page_n.desc)),
        ),
    );
    if let Some(cr) = cr {
        if let Ok(v) = cr.parse() {
            resp.headers_mut().insert("Content-Range", v);
        }
    }
    resp
}

/// 5.7.3.4 / 4.5.5: merge one remote Temporal Evolution into `base` by
/// appending instances per Attribute; auxiliary data contributes an
/// instance only when no instance with the same timeproperty value was
/// received from elsewhere.
pub(super) fn merge_temporal_docs(base: &mut Value, add: &Value, aux: bool, timeprop: &str) {
    let (Some(target), Some(source)) = (base.as_object_mut(), add.as_object()) else {
        return;
    };
    for (k, v) in source {
        if [
            "id",
            "type",
            "scope",
            "createdAt",
            "modifiedAt",
            "deletedAt",
            "expiresAt",
        ]
        .contains(&k.as_str())
        {
            target.entry(k.clone()).or_insert_with(|| v.clone());
            continue;
        }
        let incoming: Vec<Value> = match v {
            Value::Array(x) => x.clone(),
            other => vec![other.clone()],
        };
        match target.get_mut(k).and_then(Value::as_array_mut) {
            None => {
                target.insert(k.clone(), Value::Array(incoming));
            }
            Some(cur) => {
                for ni in incoming {
                    if aux {
                        let ts = ni.get(timeprop).and_then(Value::as_str).map(dt_key);
                        if ts.is_some()
                            && cur.iter().any(|ci| {
                                ci.get(timeprop).and_then(Value::as_str).map(dt_key) == ts
                            })
                        {
                            continue;
                        }
                    }
                    // 4.5.5.3: an instance with the same datasetId (or both
                    // default) AND the same timeproperty value is a
                    // CONFLICTING instance of one slot — resolve to one, the
                    // most recent modifiedAt winning.
                    // Two instances only share a slot when they carry the SAME
                    // timeproperty value. Without the is_some() guard a pair of
                    // instances that both lack it would compare None == None,
                    // letting a remote instance replace unrelated local history.
                    // 4.6.3 leaves the seconds fraction optional, so two
                    // Context Sources may spell one instant differently: the
                    // slot and its winner are decided on the canonical key,
                    // or the same instance comes back twice.
                    let ni_ts = ni.get(timeprop).and_then(Value::as_str).map(dt_key);
                    let slot = ni_ts.and_then(|ts| {
                        cur.iter_mut().find(|ci| {
                            ci.get(timeprop).and_then(Value::as_str).map(dt_key) == Some(ts.clone())
                                && ci.get("datasetId") == ni.get("datasetId")
                        })
                    });
                    if let Some(existing) = slot {
                        let stamp = |i: &Value| {
                            dt_key(i.get("modifiedAt").and_then(Value::as_str).unwrap_or(""))
                        };
                        let newer = stamp(&ni) > stamp(existing);
                        if newer {
                            *existing = ni;
                        }
                        continue;
                    }
                    cur.push(ni);
                }
            }
        }
    }
}

/// The Entity `id` whose retention keeps no history (T-5056: a type no
/// rule opts in, or `off`), as a Temporal Evolution of no Attribute: it
/// exists and recorded nothing, which is no 404. The member checks after
/// the read hold for it as for any other.
pub(super) async fn kept_no_history(
    st: &AppState,
    tenant: &TenantId,
    id: &str,
) -> Result<Option<Value>, ApiError> {
    let Some(doc) = st
        .store
        .get(tenant, antares_store::Kind::Entity, id)
        .await?
    else {
        return Ok(None);
    };
    if crate::history::records_history(st, tenant, &doc) {
        return Ok(None);
    }
    let mut shell = serde_json::Map::new();
    for k in ["id", "type", "scope"] {
        if let Some(v) = doc.get(k) {
            shell.insert(k.to_owned(), v.clone());
        }
    }
    Ok(Some(Value::Object(shell)))
}
