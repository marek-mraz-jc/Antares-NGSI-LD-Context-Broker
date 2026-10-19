// SPDX-License-Identifier: EUPL-1.2
//! /temporal/entities (5.6.11–5.6.16, 5.7.3/5.7.4; resources 6.18–6.22).

use crate::negotiate::*;
use crate::state::{now_iso, AppState};
use crate::temporalq::TemporalQ;
use antares_jsonld::compact::compact_instance;
use antares_jsonld::{expand_entity, Context, ExpandOpts};
use antares_model::{dt_key, NgsiError, TenantId};
use antares_ql::parse_q;
use antares_store::TemporalDriverExt as _;
use axum::body::Bytes;
use axum::extract::{Path, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use serde_json::{json, Map, Value};
use std::collections::HashMap;

use crate::negotiate::CleanParams;

use antares_model::is_meta;

/// 5.6.11 input: the pushed Temporal Evolution may carry the 4.5.7
/// deleted-instance representation (value = NGSI-LD Null), which 5.5.4
/// explicitly excepts for "the temporal evolution" — hence allow_null.
const TEMPORAL_OPTS: ExpandOpts = ExpandOpts {
    fragment: false,
    allow_null: true,
    merge: false,
    temporal: true,
    sys: false,
};

/// 4.5.7 and 4.5.8: each recorded instance of a Property or Relationship
/// carries an instanceId and the 4.8 timestamps. The id is minted here and
/// only when the pushed instance brought none, because the temporal API is
/// add-only (5.6.11.4, 5.6.12.1) and 5.6.14/5.6.15 address an instance by
/// that id. Sub-Attributes are part of the instance and are not stamped
/// separately -- the current-state path stamps them one level deeper
/// (`crate::stamp::stamp_instances`).
fn stamp_temporal_instances(doc: &mut Value, ts: &str) {
    if let Some(obj) = doc.as_object_mut() {
        for (k, v) in obj.iter_mut() {
            if is_meta(k) {
                continue;
            }
            if let Some(arr) = v.as_array_mut() {
                for inst in arr {
                    if let Some(o) = inst.as_object_mut() {
                        o.entry("instanceId".to_owned()).or_insert_with(|| {
                            Value::String(format!("urn:ngsi-ld:Instance:{}", uuid::Uuid::new_v4()))
                        });
                        o.insert("createdAt".into(), Value::String(ts.to_owned()));
                        o.insert("modifiedAt".into(), Value::String(ts.to_owned()));
                    }
                }
            }
        }
    }
}

// ---------- POST /temporal/entities/ — Upsert temporal (5.6.11) ----------

pub async fn upsert_temporal(
    State(st): State<AppState>,
    CleanParams(params): CleanParams,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let go = async {
        let tenant = tenant_from(&headers)?;
        check_params(&params, &["options", "local"])?;
        let parsed = parse_body(&st.loader, &headers, &body, BodyKind::Standard).await?;
        let obj = parsed.object(NgsiError::BadRequestData(
            "temporal entity must be a JSON object".into(),
        ))?;
        let expanded = expand_entity(obj, &parsed.ctx, TEMPORAL_OPTS)?;
        let id = antares_jsonld::expanded_id(&expanded)?.to_owned();
        gate!(st, &tenant, &headers, "5.6.11", ids: &[&id]).await?;
        // 5.6.11.4: exclusive/redirect registrations matching the input are
        // forwarded when "Create or Update Temporal" is supported; proxy
        // modes without it are an error of type Conflict; inclusive ones
        // forward when supported. Matching attributes are removed from the
        // local fragment.
        let spec = crate::registry::CsrSpec {
            ids: Some(vec![id.clone()]),
            ..Default::default()
        };
        let regs = match crate::federation::write_plan(
            &st,
            &tenant,
            &spec,
            &parsed.ctx,
            &params,
            &headers,
        )
        .await?
        {
            crate::federation::WritePlan::Answered(r) => return Ok(*r),
            crate::federation::WritePlan::Forward(regs) => regs,
        };
        if !regs.is_empty() {
            let mut parts = Vec::new();
            let mut fwd = Vec::new();
            for reg in &regs {
                if !reg.supports("upsertTemporal") {
                    if reg.is_proxy() {
                        parts.push(crate::federation::conflict_part("upsertTemporal"));
                    }
                    continue;
                }
                if let Some(frag) = crate::federation::reduce_to_scope(obj, reg, &parsed.ctx) {
                    fwd.push((reg.clone(), frag));
                }
            }
            let proxies: Vec<&crate::federation::FedReg> =
                regs.iter().filter(|r| r.is_proxy()).collect();
            let (rest, has_attrs) = crate::federation::strip_proxied(obj, &proxies, &parsed.ctx);
            if has_attrs || proxies.is_empty() {
                let local = expand_entity(&rest, &parsed.ctx, TEMPORAL_OPTS)?;
                let status = upsert_temporal_local(&st, &tenant, &id, local).await?;
                parts.push(crate::federation::Part {
                    status: status.as_u16(),
                    detail: "local temporal upsert".into(),
                });
            }
            let ctx_url = crate::federation::ctx_link_url(&headers, &parsed.ctx.source);
            for (reg, frag) in fwd {
                parts.push(
                    crate::federation::forward_part(
                        &st,
                        reqwest::Method::POST,
                        format!("{}/ngsi-ld/v1/temporal/entities", reg.endpoint),
                        &[],
                        &headers,
                        &tenant,
                        &reg,
                        &ctx_url,
                        Some(frag),
                    )
                    .await,
                );
            }
            return Ok(crate::federation::combine(
                parts,
                created(
                    format!(
                        "/ngsi-ld/v1/temporal/entities/{}",
                        crate::federation::path_segment(&id)
                    ),
                    &tenant,
                ),
                &tenant,
            ));
        }
        let status = upsert_temporal_local(&st, &tenant, &id, expanded).await?;
        Ok::<_, ApiError>(if status == StatusCode::CREATED {
            created(
                format!(
                    "/ngsi-ld/v1/temporal/entities/{}",
                    crate::federation::path_segment(&id)
                ),
                &tenant,
            )
        } else {
            no_content(&tenant)
        })
    };
    go.await.unwrap_or_else(|e| e.into_response())
}

/// 5.6.11.4 local half: create the Temporal Evolution, or add the provided
/// instances to the existing one per 5.6.12 (merge key = datasetId +
/// observedAt) with Entity Type names unioned. Returns 201 vs 204.
async fn upsert_temporal_local(
    st: &AppState,
    tenant: &antares_model::TenantId,
    id: &str,
    mut expanded: Value,
) -> ApiResult<StatusCode> {
    let ts = now_iso();
    stamp_temporal_instances(&mut expanded, &ts);
    // get->create/mutate is a TOCTOU pair: two concurrent first-upserts
    // both see "absent", and the loser's create must NOT be silently
    // dropped (201 with a discarded payload). Loop: a lost create retries
    // as a merge, a mutate on a just-deleted doc retries as a create.
    let mut attempts: u8 = 0;
    loop {
        attempts = attempts.saturating_add(1);
        if attempts > 16 {
            return Err(NgsiError::InternalError("upsert retry storm".into()).into());
        }
        let existed = st.temporal.get(tenant, id).await?.is_some();
        if existed {
            let res = st
                .temporal
                .mutate(tenant, id, |doc| {
                    let target = antares_store::stored_object(doc)?;
                    // 5.6.11.4: new Entity Type names are added to the target
                    if let Some(new_types) = expanded.get("type").and_then(Value::as_array) {
                        let mut cur: Vec<Value> = target
                            .get("type")
                            .and_then(Value::as_array)
                            .cloned()
                            .unwrap_or_default();
                        for t in new_types {
                            if !cur.contains(t) {
                                cur.push(t.clone());
                            }
                        }
                        target.insert("type".into(), Value::Array(cur));
                    }
                    // 5.6.11.4: the instances "shall be added" as 5.6.12
                    // mandates; identity is the instanceId, never observedAt
                    antares_jsonld::expanded_object(&expanded)?;
                    add_temporal_instances(doc, &expanded, &ts);
                    Ok::<(), NgsiError>(())
                })
                .await?;
            match res {
                Some(Err(e)) => return Err(ApiError::from(e)),
                Some(Ok(())) => return Ok(StatusCode::NO_CONTENT),
                None => continue, // deleted between get and mutate - retry as create
            }
        } else {
            let mut doc = expanded.clone();
            if let Some(o) = doc.as_object_mut() {
                o.insert("createdAt".into(), Value::String(ts.clone()));
                o.insert("modifiedAt".into(), Value::String(ts.clone()));
            }
            if st.temporal.create(tenant, id, doc).await? {
                return Ok(StatusCode::CREATED);
            }
            // lost the create race - the doc exists now; retry as a merge
        }
    }
}

// ---------- temporal query params (4.11) ----------

/// Windowed per-entity temporal data: filtered+ordered instances per attr.
struct Windowed {
    attrs: std::collections::BTreeMap<String, Vec<Value>>,
    max_per_attr: usize,
    ts_min: Option<String>,
    ts_max: Option<String>,
    truncated: bool,
    /// some Attribute holds instances past this page in its order: the
    /// response links to the next page (6.4.7.3)
    more: bool,
    /// the fewest instances an Attribute the ceiling cut shortened still
    /// served (`truncate`): how far the next offsetN page may move
    cut_served: Option<usize>,
    /// Attributes their type records `onChange` (ADR-N-054 2.5): each value
    /// holds until the next row, so a window starts at the level carried in
    /// and an aggregation weighs a value by the time it held
    held: std::collections::HashSet<String>,
}

/// NGSI-LD 6.3.10: the most instances of one Attribute the broker serves in
/// one response — beyond it the representation is cut and answered "206" with
/// a Content-Range. The ETSI suite triggers 206 at 20 instances and expects
/// 200 at <=5, so any limit in (5,20) is spec-valid; 9 keeps margin.
const TEMPORAL_INSTANCE_LIMIT: usize = 9;

/// The most periods an aggregated `onChange` Attribute answers, rows and held gaps together.
const MAX_HELD_BUCKETS: usize = 10_000;

/// 6.3.10: the ceiling is a CUT, not a label. An Attribute holding more
/// instances than the broker serves at once is truncated to the ceiling in
/// the query direction, and the 206 + Content-Range then describes the
/// instances actually returned. This is what caps a lastN above the ceiling
/// and what caps a request naming no temporal window at all. Aggregated
/// representations (5.7.4.4) are computed over the whole evolution and are
/// complete by construction, so they are never cut.
///
/// The cut is ONE time boundary for the whole entity, not a per-attribute
/// count: the partial content "shall" be the representation the
/// Content-Range describes, so every attribute is trimmed to the tightest
/// ceiling instant among the over-full ones (ties at that instant kept),
/// and a client continuing from the advertised range-end misses no instance
/// of any attribute. An attribute lying entirely beyond the boundary comes
/// back empty on this page. ETSI 020_14 checks exactly that (T-3612; the
/// per-Attribute count of T-3456 failed it on every store).
///
/// An Attribute the boundary cut shorter than the ceiling holds the next
/// page's instances at a smaller offset than the boundary Attribute, so the
/// next offsetN Link moves by the fewest instances a cut Attribute served
/// (`cut_served`): it repeats, never skips. One cut to nothing leaves no
/// offsetN that continues it, and the Link continues by time from the
/// boundary instead (`interval_links`, T-3613).
fn truncate(w: &mut Windowed, timeprop: &str, descending: bool) {
    if w.max_per_attr <= TEMPORAL_INSTANCE_LIMIT {
        return;
    }
    w.truncated = true;
    w.more = true;
    w.max_per_attr = TEMPORAL_INSTANCE_LIMIT;
    let key = |inst: &Value| inst.get(timeprop).and_then(Value::as_str).map(dt_key);
    // the tightest ceiling instant: earliest forwards, latest backwards
    let boundary = w
        .attrs
        .values()
        .filter(|insts| insts.len() > TEMPORAL_INSTANCE_LIMIT)
        .filter_map(|insts| insts.get(TEMPORAL_INSTANCE_LIMIT - 1).and_then(key))
        .reduce(|a, b| if (b < a) != descending { b } else { a });
    let (mut ts_min, mut ts_max) = (None::<String>, None::<String>);
    for instances in w.attrs.values_mut() {
        let before = instances.len();
        match &boundary {
            Some(bd) => instances.retain(|inst| {
                key(inst).is_none_or(|k| if descending { k >= *bd } else { k <= *bd })
            }),
            None => instances.truncate(TEMPORAL_INSTANCE_LIMIT),
        }
        if instances.len() < before {
            w.cut_served = Some(
                w.cut_served
                    .map_or(instances.len(), |c| c.min(instances.len())),
            );
        }
        for inst in instances.iter() {
            if let Some(t) = inst.get(timeprop).and_then(Value::as_str) {
                if ts_min.as_deref().is_none_or(|m| dt_key(t) < dt_key(m)) {
                    ts_min = Some(t.to_owned());
                }
                if ts_max.as_deref().is_none_or(|m| dt_key(t) > dt_key(m)) {
                    ts_max = Some(t.to_owned());
                }
            }
        }
    }
    (w.ts_min, w.ts_max) = (ts_min, ts_max);
}

/// Each instance of `w` replaced by its chain of versions in `chains` (the
/// store's `versions`), the same measurement being the same Attribute,
/// `observedAt` and `datasetId` (ADR-N-054 2.4). An instance the store keeps
/// no version of stays as it is.
fn with_versions(w: &mut Windowed, chains: &Map<String, Value>) {
    let key = |i: &Value| {
        (
            i.get("observedAt").and_then(Value::as_str).map(dt_key),
            i.get("datasetId").cloned(),
        )
    };
    for (attr, instances) in &mut w.attrs {
        let Some(chain) = chains.get(attr).and_then(Value::as_array) else {
            continue;
        };
        *instances = instances
            .iter()
            .flat_map(|inst| {
                let own: Vec<Value> = chain
                    .iter()
                    .filter(|v| key(v) == key(inst))
                    .cloned()
                    .collect();
                if own.is_empty() {
                    vec![inst.clone()]
                } else {
                    own
                }
            })
            .collect();
    }
}

/// The instant a cut page ends at in its order: the boundary `truncate`
/// kept every Attribute to.
fn cut_boundary(w: &Windowed, desc: bool) -> Option<&str> {
    if desc {
        w.ts_min.as_deref()
    } else {
        w.ts_max.as_deref()
    }
}

/// 5.7.4.4 S4/S7: does a scope VALUE (string or array of strings) match the
/// 4.19 Scope query? The 4.5.7 deletion sentinel never matches.
fn scope_value_matches(sq: &str, v: &Value) -> bool {
    if v.is_null() || v.as_str() == Some("urn:ngsi-ld:null") {
        return false;
    }
    crate::scope_matches(sq, &serde_json::json!({ "scope": v }))
}

/// 4.18 over a Temporal Evolution: "a given Scope is considered valid from
/// the time it has been set until the time it has been explicitly removed by
/// an update or delete operation" (example: annex C.5.16). Instance-shaped
/// scope arrays become [set-time, next-set-time) validity intervals
/// (set-time = observedAt‖modifiedAt‖createdAt — 4.5.6 mirrors them from the
/// Core API change); a plain string/array scope is valid for all time.
/// Returns only the intervals whose value matches `sq`; "" start = -inf,
/// None end = +inf.
fn scope_match_intervals(doc: &Value, sq: &str) -> Vec<(String, Option<String>)> {
    match doc.get("scope") {
        Some(Value::Array(a)) if a.first().is_some_and(Value::is_object) => {
            let mut states: Vec<(&str, &Value)> = a
                .iter()
                .filter_map(|i| {
                    scope_set_time(i).map(|t| (t, i.get("value").unwrap_or(&Value::Null)))
                })
                .collect();
            states.sort_by_key(|(t, _)| dt_key(t));
            // each state with the time the next one was set, the last open
            let next = states.iter().skip(1).map(Some).chain(std::iter::once(None));
            states
                .iter()
                .zip(next)
                .filter(|((_, value), _)| scope_value_matches(sq, value))
                .map(|((t, _), next)| ((*t).to_owned(), next.map(|(t, _)| (*t).to_owned())))
                .collect()
        }
        Some(_) if crate::scope_matches(sq, doc) => vec![(String::new(), None)],
        _ => Vec::new(),
    }
}

/// The time a temporal scope instance was set (4.5.6: observedAt is a copy
/// of modifiedAt on Core-API changes; direct 5.6.11 input may carry any).
fn scope_set_time(i: &Value) -> Option<&str> {
    ["observedAt", "modifiedAt", "createdAt"]
        .iter()
        .find_map(|k| i.get(*k).and_then(Value::as_str))
}

// seven of the request's own plus the Attributes the type records onChange
#[allow(clippy::too_many_arguments)]
fn window(
    doc: &Value,
    tq: Option<&TemporalQ>,
    page: InstancePage,
    attrs_filter: Option<&Vec<String>>,
    omit: Option<&Vec<crate::repr::ProjNode>>,
    dataset: Option<&Vec<String>>,
    timeprop: &str,
    held: std::collections::HashSet<String>,
) -> Windowed {
    let mut w = Windowed {
        attrs: std::collections::BTreeMap::new(),
        max_per_attr: 0,
        ts_min: None,
        ts_max: None,
        truncated: false,
        cut_served: None,
        more: false,
        held,
    };
    let Some(obj) = doc.as_object() else { return w };
    for (k, v) in obj {
        // 4.5.6: the Scope of a Temporal Evolution is represented as the
        // temporal representation of a Property — instance-shaped scope
        // arrays window like attributes (plain-string scope stays meta).
        let scope_instances = k == "scope"
            && v.as_array()
                .is_some_and(|a| a.first().is_some_and(Value::is_object));
        if is_meta(k) && !scope_instances {
            continue;
        }
        if let Some(want) = attrs_filter {
            if !want.contains(k) {
                continue;
            }
        }
        if let Some(omit) = omit {
            if omit.iter().any(|n| n.iri == *k && n.children.is_none()) {
                continue;
            }
        }
        let of_dataset = |inst: &&Value| match (dataset, inst.get("datasetId")) {
            (None, _) => true,
            (Some(want), Some(Value::String(have))) => want.iter().any(|w| w == have),
            (Some(want), None) => want.iter().any(|w| w == "@none"),
            _ => false,
        };
        let mut instances: Vec<Value> = v
            .as_array()
            .into_iter()
            .flatten()
            .filter(of_dataset)
            .filter(|inst| tq.is_none_or(|tq| tq.instance_matches(inst)))
            .cloned()
            .collect();
        // ADR-N-054 2.5 (DM-117): an `onChange` Attribute held its last row
        // before the window when the window opened, so that row comes along.
        let start = tq.and_then(|t| match t.timerel.as_str() {
            "after" | "between" => Some(dt_key(&t.time_at)),
            _ => None,
        });
        if let (true, Some(start)) = (w.held.contains(k), &start) {
            let key = |i: &&Value| i.get(timeprop).and_then(Value::as_str).map(dt_key);
            let carry = v
                .as_array()
                .into_iter()
                .flatten()
                .filter(of_dataset)
                .filter(|i| key(i).is_some_and(|t| t < *start))
                .max_by_key(key);
            if let Some(c) = carry {
                instances.push(c.clone());
            }
        }
        // 4.18/C.5.16: the scope valid AT the window start was set at or
        // before it — carry the latest pre-window instance into the
        // representation (a temporal scope stays valid until replaced).
        if scope_instances {
            let start = tq.and_then(|t| match t.timerel.as_str() {
                "after" | "between" => Some(t.time_at.as_str()),
                _ => None,
            });
            if let Some(start) = start {
                let carry = v
                    .as_array()
                    .into_iter()
                    .flatten()
                    .filter(|i| scope_set_time(i).is_some_and(|t| dt_key(t) < dt_key(start)))
                    .max_by_key(|i| scope_set_time(i).map(dt_key))
                    .cloned();
                if let Some(c) = carry {
                    if !instances.contains(&c) {
                        instances.push(c);
                    }
                }
            }
        }
        instances.sort_by(|a, b| {
            // Canonicalize before comparing: '.' sorts before 'Z', so a raw
            // string compare puts "…00.5Z" ahead of "…00Z" and lastN keeps the
            // wrong instant. 4.6.3 allows both fraction spellings.
            let ta = a.get(timeprop).and_then(Value::as_str).unwrap_or("");
            let tb = b.get(timeprop).and_then(Value::as_str).unwrap_or("");
            dt_key(ta).cmp(&dt_key(tb))
        });
        // 6.4.7.3: lastN pages newest first, firstN and neither oldest first
        if page.desc {
            instances.reverse();
        }
        // Table 6.4.7.3-2: an offsetN past the end "should be assumed to be
        // equal to the size of the result set, i.e. only the last element
        // of the result set is to be returned"
        let skip = page.offset.min(instances.len().saturating_sub(1));
        instances.drain(..skip);
        if let Some(n) = page.n {
            if instances.len() > n {
                instances.truncate(n);
                w.more = true;
            }
        }
        if instances.is_empty() {
            continue;
        }
        w.max_per_attr = w.max_per_attr.max(instances.len());
        for inst in &instances {
            if let Some(t) = inst.get(timeprop).and_then(Value::as_str) {
                if w.ts_min.as_deref().is_none_or(|m| dt_key(t) < dt_key(m)) {
                    w.ts_min = Some(t.to_owned());
                }
                if w.ts_max.as_deref().is_none_or(|m| dt_key(t) > dt_key(m)) {
                    w.ts_max = Some(t.to_owned());
                }
            }
        }
        w.attrs.insert(k.clone(), instances);
    }
    w
}

/// `Content-Range: date-time <start>-<end>/<size>` (Scorpio-parity semantics).
fn content_range(
    truncated: bool,
    ts_min: Option<&str>,
    ts_max: Option<&str>,
    tq: Option<&TemporalQ>,
    page: InstancePage,
) -> Option<String> {
    if !truncated {
        return None;
    }
    let (data_min, data_max) = (ts_min?, ts_max?);
    // The window bound is the query's own when the query names one; matching
    // on the pair keeps the timerel and the query that produced it together,
    // so no arm can reach for a query that is not there.
    let named = tq.filter(|t| t.timerel != "any");
    let (start, end) = if !page.desc {
        let start = match named.map(|t| (t.timerel.as_str(), t)) {
            Some(("after" | "between", t)) => t.time_at.clone(),
            _ => data_min.to_owned(),
        };
        (start, data_max.to_owned())
    } else {
        let start = match named.map(|t| (t.timerel.as_str(), t)) {
            Some(("before", t)) => t.time_at.clone(),
            Some(("between", t)) => t.end_time_at.clone().unwrap_or_else(|| data_max.to_owned()),
            _ => data_max.to_owned(),
        };
        (start, data_min.to_owned())
    };
    // start/end bound the instances actually returned; the size is the length
    // of the complete representation the client asked for — the requested
    // firstN or lastN, or "*" when the window leaves it unknown.
    let size = page.n.map_or_else(|| "*".to_owned(), |n| n.to_string());
    Some(format!("date-time {start}-{end}/{size}"))
}

/// TS 104 176 6.4.7.3: the Links to the neighbouring instance pages, each
/// the request with offsetN moved by one page and typed with the response
/// media type. Ascending pages run forward in time, so their next page is
/// `intervalafter` and their previous one `intervalbefore`; lastN pages run
/// backward and swap the two. `step` is the page size served per Attribute.
///
/// A page the ceiling cut moves its next Link by `cut`, the fewest
/// instances a cut Attribute served (see `truncate`). When that is none at
/// all, no offsetN continues every Attribute, and the next Link asks for the
/// time past `boundary` instead (the window's end the cut moved): `after`
/// it ascending, which repeats the instances at the boundary instant once
/// (4.11 `after` is inclusive), `before` it descending, which is exact.
fn interval_links(
    params: &HashMap<String, String>,
    path: &str,
    accept: Accept,
    page: InstancePage,
    more: bool,
    step: usize,
    cut: Option<(usize, &str)>,
) -> Vec<String> {
    let (forward, back) = match page.desc {
        false => ("intervalafter", "intervalbefore"),
        true => ("intervalbefore", "intervalafter"),
    };
    // the request with `set` replacing its own values of those names
    let link = |set: Vec<(&str, String)>, rel: &str| {
        let mut qp: Vec<String> = params
            .iter()
            .filter(|(k, _)| k.as_str() != "offsetN" && !set.iter().any(|(s, _)| s == k))
            .map(|(k, v)| (k.as_str(), v.as_str()))
            .chain(set.iter().map(|(k, v)| (*k, v.as_str())))
            .map(|(k, v)| format!("{k}={}", crate::paging::query_value(v)))
            .collect();
        qp.sort();
        format!(
            "<{path}?{}>; rel=\"{rel}\";type=\"{}\"",
            qp.join("&"),
            crate::paging::link_media_type(accept)
        )
    };
    let offset = |n: usize| vec![("offsetN", n.to_string())];
    let mut links = Vec::new();
    if more {
        match cut {
            None => links.push(link(offset(page.offset.saturating_add(step)), forward)),
            Some((0, boundary)) => {
                links.push(link(resume_after(params, page.desc, boundary), forward))
            }
            Some((served, _)) => {
                links.push(link(
                    offset(page.offset.saturating_add(served.min(step))),
                    forward,
                ));
            }
        }
    }
    if page.offset > 0 {
        links.push(link(offset(page.offset.saturating_sub(step)), back));
    }
    links
}

/// ponytail: ascending, a page whose boundary instant holds a whole page of
/// one Attribute's instances links to itself; offsetN by time would fix it.
///
/// The request's time window moved past `boundary` in the page order: the
/// window's other end stays (4.11: `between` is inclusive of `timeAt`,
/// exclusive of `endTimeAt`).
fn resume_after(
    params: &HashMap<String, String>,
    desc: bool,
    boundary: &str,
) -> Vec<(&'static str, String)> {
    let rel = params.get("timerel").map(String::as_str);
    let (at, end) = (params.get("timeAt"), params.get("endTimeAt"));
    let b = boundary.to_owned();
    let between = |from: String, to: String| {
        vec![
            ("timerel", "between".to_owned()),
            ("timeAt", from),
            ("endTimeAt", to),
        ]
    };
    match (desc, rel, at, end) {
        (false, Some("between"), _, Some(e)) | (false, Some("before"), Some(e), _) => {
            between(b, e.clone())
        }
        (false, ..) => vec![("timerel", "after".to_owned()), ("timeAt", b)],
        (true, Some("between" | "after"), Some(a), _) => between(a.clone(), b),
        (true, ..) => vec![("timerel", "before".to_owned()), ("timeAt", b)],
    }
}

/// Render one temporal entity from its windowed data.
fn present_temporal(
    doc: &Value,
    w: &Windowed,
    ctx: &Context,
    r: &TRepr,
    tq: Option<&TemporalQ>,
    timeprop: &str,
) -> Result<Value, NgsiError> {
    let Some(obj) = doc.as_object() else {
        return Ok(doc.clone());
    };
    let mut out = Map::new();
    for (k, v) in obj {
        let scope_instances = k == "scope"
            && v.as_array()
                .is_some_and(|a| a.first().is_some_and(Value::is_object));
        if is_meta(k) && !scope_instances {
            match k.as_str() {
                // Table 6.3.11-1: `sysAttrs` is what admits "the system
                // generated temporal attributes createdAt, modifiedAt and
                // the system temporal attribute expiresAt … In the case of
                // temporal representations, also the system generated
                // temporal attribute deletedAt". Without it none of them is
                // in the payload — the same set `repr.rs` gates on the
                // current-state path.
                "createdAt" | "modifiedAt" | "expiresAt" | "deletedAt" if !r.sys => continue,
                _ => {}
            }
            if !crate::repr::meta_projected(r.pick.as_deref(), r.omit.as_deref(), k) {
                continue;
            }
            if k == "type" {
                out.insert("type".into(), antares_jsonld::compact_types(v, ctx));
            } else {
                out.insert(k.clone(), v.clone());
            }
        }
    }
    if r.aggregated {
        for (k, v) in render_aggregated(w, tq, r, ctx, timeprop)? {
            out.insert(k, v);
        }
        return Ok(Value::Object(out));
    }
    for (k, instances) in &w.attrs {
        // Table 6.18.3.2-1 / 6.19.3.1 `lang`: each LanguageProperty
        // instance becomes a Property in the chosen language (4.15) before
        // either representation renders it.
        let reduced: Vec<Value>;
        let instances: &[Value] = match &r.lang {
            Some(lang) => {
                reduced = instances
                    .iter()
                    .map(|inst| {
                        let mut inst = inst.clone();
                        if let Some(o) = inst.as_object_mut() {
                            crate::repr::apply_lang(o, lang);
                        }
                        inst
                    })
                    .collect();
                &reduced
            }
            None => instances,
        };
        if instances.is_empty() {
            // gap-cut leftovers render as empty arrays
            out.insert(ctx.compact_iri(k), Value::Array(vec![]));
            continue;
        }
        if r.temporal_values {
            // group instances by datasetId (4.5.9)
            let mut groups: Vec<(Option<String>, Vec<&Value>)> = Vec::new();
            for inst in instances {
                let ds = inst
                    .get("datasetId")
                    .and_then(Value::as_str)
                    .map(String::from);
                match groups.iter_mut().find(|(g, _)| *g == ds) {
                    Some((_, list)) => list.push(inst),
                    None => groups.push((ds, vec![inst])),
                }
            }
            let mut rendered: Vec<Value> = groups
                .iter()
                .map(|(ds, list)| {
                    let atype = list
                        .first()
                        .and_then(|i| i.get("type"))
                        .cloned()
                        .unwrap_or_else(|| Value::String("Property".into()));
                    let values: Vec<Value> = list
                        .iter()
                        .map(|inst| {
                            // 4.5.9: Property/Relationship pairs carry the bare
                            // value/object; other attribute kinds wrap it under
                            // their member name.
                            let v = if let Some(v) = inst.get("value") {
                                // 4.5.9: "the first element shall be a
                                // Property value" — the one the instance
                                // holds. A f64 round trip would retype an
                                // integer and drop a digit past 2^53.
                                v.clone()
                            } else if let Some(o) = inst.get("object") {
                                o.clone()
                            } else if let Some(lm) = inst.get("languageMap") {
                                serde_json::json!({"languageMap": lm})
                            } else if let Some(j) = inst.get("json") {
                                serde_json::json!({"json": j})
                            } else if let Some(vv) = inst.get("vocab") {
                                let compacted = match vv {
                                    Value::String(iri) => Value::String(ctx.compact_iri(iri)),
                                    Value::Array(a) => Value::Array(
                                        a.iter()
                                            .map(|s| match s {
                                                Value::String(iri) => {
                                                    Value::String(ctx.compact_iri(iri))
                                                }
                                                o => o.clone(),
                                            })
                                            .collect(),
                                    ),
                                    o => o.clone(),
                                };
                                serde_json::json!({"vocab": compacted})
                            } else if let Some(l) = inst.get("valueList") {
                                // 4.5.9 p.63 EXAMPLE 3: the pair's first element
                                // is the BARE ordered array, not a {"valueList"}
                                // wrapper — unlike languageMap/
                                // json/vocab, which the clause does wrap
                                l.clone()
                            } else if let Some(l) = inst.get("objectList") {
                                // 4.5.9 p.65: same bare form for ListRelationship
                                l.clone()
                            } else {
                                Value::Null
                            };
                            let t = inst.get(timeprop).cloned().unwrap_or(Value::Null);
                            Value::Array(vec![v, t])
                        })
                        .collect();
                    let mut o = Map::new();
                    // 4.5.9: the simplified member name follows the attribute type
                    let member = match atype.as_str() {
                        Some("Relationship") => "objects",
                        Some("LanguageProperty") => "languageMaps",
                        Some("VocabProperty") => "vocabs",
                        Some("JsonProperty") => "jsons",
                        Some("ListProperty") => "valueLists",
                        Some("ListRelationship") => "objectLists",
                        _ => "values",
                    };
                    o.insert("type".into(), atype);
                    if let Some(ds) = ds {
                        o.insert("datasetId".into(), Value::String(ds.clone()));
                    }
                    o.insert(member.into(), Value::Array(values));
                    Value::Object(o)
                })
                .collect();
            let rendered = if rendered.len() == 1 {
                rendered.remove(0)
            } else {
                Value::Array(rendered)
            };
            out.insert(ctx.compact_iri(k), rendered);
        } else {
            let presented: Vec<Value> = instances
                .iter()
                .map(|inst| {
                    let mut ci = inst.clone();
                    if !r.sys {
                        if let Some(o) = ci.as_object_mut() {
                            o.remove("createdAt");
                            o.remove("modifiedAt");
                            // 6.3.11: expiresAt is sysAttrs-gated too
                            o.remove("expiresAt");
                            // ... and so are the sub-Attributes' own (4.8)
                            for sub in o.values_mut() {
                                let subs = match sub {
                                    Value::Array(a) => a.iter_mut().collect(),
                                    other => vec![other],
                                };
                                for so in subs.into_iter().filter_map(Value::as_object_mut) {
                                    if so.contains_key("type") {
                                        so.remove("createdAt");
                                        so.remove("modifiedAt");
                                    }
                                }
                            }
                        }
                    }
                    compact_instance(&ci, ctx)
                })
                .collect();
            out.insert(ctx.compact_iri(k), Value::Array(presented));
        }
    }
    Ok(Value::Object(out))
}

/// TS 104 176 6.4.7.3: which instances of each Attribute one response
/// carries. firstN pages oldest first, lastN newest first, neither oldest
/// first; offsetN skips that many in the chosen order.
#[derive(Default, Clone, Copy, Debug, PartialEq)]
pub(crate) struct InstancePage {
    /// lastN: newest first
    desc: bool,
    /// the firstN or lastN asked for
    n: Option<usize>,
    /// offsetN
    offset: usize,
}

impl InstancePage {
    /// firstN, lastN and offsetN, validated: N positive (5.2.21), offsetN
    /// not negative (Table 6.4.7.3-2), both at most i64::MAX (bound as a
    /// `bigint` they would wrap negative and silently select nothing).
    /// firstN asks for ascending order and lastN for descending; one page
    /// cannot be both, so the pair is BadRequestData.
    fn parse(params: &HashMap<String, String>) -> Result<Self, NgsiError> {
        let count = |name: &str, min: usize| -> Result<Option<usize>, NgsiError> {
            params
                .get(name)
                .map(|v| {
                    v.parse::<usize>()
                        .ok()
                        .filter(|n| *n >= min && *n <= i64::MAX as usize)
                        .ok_or_else(|| NgsiError::BadRequestData(format!("invalid {name} {v:?}")))
                })
                .transpose()
        };
        let (first, last) = (count("firstN", 1)?, count("lastN", 1)?);
        if first.is_some() && last.is_some() {
            return Err(NgsiError::BadRequestData(
                "firstN pages ascending and lastN descending: give one of them (6.4.7.3)".into(),
            ));
        }
        Ok(Self {
            desc: last.is_some(),
            n: first.or(last),
            offset: count("offsetN", 0)?.unwrap_or(0),
        })
    }

    /// The most instances of one Attribute a page holds: N, or the broker's
    /// ceiling when N is above it or absent (6.4.7.3 "whichever is smaller").
    fn size(&self) -> usize {
        self.n
            .map_or(TEMPORAL_INSTANCE_LIMIT, |n| n.min(TEMPORAL_INSTANCE_LIMIT))
    }

    /// How far one page moves offsetN: its size, or N itself for an
    /// aggregated representation, which the ceiling never cuts.
    fn step(&self, aggregated: bool) -> usize {
        match (aggregated, self.n) {
            (true, Some(n)) => n,
            _ => self.size(),
        }
    }

    /// The per-Attribute cap a store may apply before the window: every
    /// instance up to the end of this page in its order, and one more, so
    /// the window still sees whether a next page exists.
    fn store_cap(&self) -> Option<i64> {
        self.n.map(|n| {
            n.saturating_add(self.offset)
                .saturating_add(1)
                .min(i64::MAX as usize) as i64
        })
    }
}

/// Parsed temporal representation params (options/format/lastN/pick/omit/
/// datasetId/aggregation), fully validated up front.
#[derive(Default, Clone)]
pub(crate) struct TRepr {
    temporal_values: bool,
    aggregated: bool,
    sys: bool,
    page: InstancePage,
    pick: Option<Vec<crate::repr::ProjNode>>,
    omit: Option<Vec<crate::repr::ProjNode>>,
    dataset_id: Option<Vec<String>>,
    attrs: Option<Vec<String>>,
    aggr_methods: Vec<String>,
    aggr_period: AggrPeriod,
    lang: Option<String>,
}

#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub(crate) enum AggrPeriod {
    /// PT0S / absent: one bucket over the whole range
    #[default]
    Whole,
    Seconds(i64),
    /// 4.5.19.1: a period may mix date and time elements
    /// ("P3Y6M4DT12H30M5S"), so a month step carries the leftover seconds —
    /// months are not a fixed number of seconds and cannot be folded in.
    Months(u32, i64),
}

pub(crate) fn parse_aggr_period(s: &str) -> Option<AggrPeriod> {
    let d = antares_model::parse_iso_duration(s)?;
    // saturating: an absurd magnitude must not panic (debug) or wrap
    // (release) — f64→int `as` casts already saturate, guard the ops.
    let months = (d.years as u32)
        .saturating_mul(12)
        .saturating_add(d.months as u32);
    let secs = [
        (d.weeks, 604_800.0),
        (d.days, 86_400.0),
        (d.hours, 3_600.0),
        (d.minutes, 60.0),
        (d.seconds, 1.0),
    ]
    .into_iter()
    .fold(0i64, |acc, (n, per)| acc.saturating_add((n * per) as i64));
    Some(match (months, secs) {
        (0, 0) => AggrPeriod::Whole,
        (0, sc) => AggrPeriod::Seconds(sc),
        (m, sc) => AggrPeriod::Months(m, sc),
    })
}

const AGGR_METHODS: &[&str] = &[
    "totalCount",
    "distinctCount",
    "sum",
    "avg",
    "min",
    "max",
    "stddev",
    "sumsq",
];

/// 5.7.3.4 / 5.7.4.4: "If projection attributes are present and indicate the
/// use of Linked Entity retrieval, an error of type BadRequestData shall be
/// raised." Unconditional on both temporal consumption operations, because
/// neither defines a join; only the clause number in the message differs.
fn reject_linked_projection(trepr: &TRepr, clause: &str) -> Result<(), NgsiError> {
    let depth = |p: &Option<Vec<crate::repr::ProjNode>>| {
        p.as_deref().map(crate::repr::proj_depth).unwrap_or(0)
    };
    if depth(&trepr.pick) > 0 || depth(&trepr.omit) > 0 {
        return Err(NgsiError::BadRequestData(format!(
            "temporal projection must not use Linked Entity selection ({clause})"
        )));
    }
    Ok(())
}

pub(crate) fn parse_trepr(
    params: &HashMap<String, String>,
    ctx: &Context,
) -> Result<TRepr, NgsiError> {
    let mut r = TRepr {
        lang: params.get("lang").cloned(),
        ..TRepr::default()
    };
    if let Some(opts) = params.get("options") {
        for o in opts.split(',') {
            match o.trim() {
                "sysAttrs" => r.sys = true,
                "temporalValues" => r.temporal_values = true,
                "aggregatedValues" => r.aggregated = true,
                "normalized" => {}
                other => {
                    return Err(NgsiError::InvalidRequest(format!(
                        "unsupported options value {other:?}"
                    )))
                }
            }
        }
    }
    // format wins over options on conflict (6.3.7)
    if let Some(f) = params.get("format") {
        match f.as_str() {
            "temporalValues" => {
                r.temporal_values = true;
                r.aggregated = false;
            }
            "aggregatedValues" => {
                r.aggregated = true;
                r.temporal_values = false;
            }
            "normalized" => {
                r.temporal_values = false;
                r.aggregated = false;
            }
            other => {
                return Err(NgsiError::InvalidRequest(format!(
                    "unsupported format value {other:?}"
                )))
            }
        }
    }
    crate::repr::check_projection_exclusive(params)?;
    if let Some(pck) = params.get("pick") {
        r.pick = Some(crate::repr::parse_projection(pck, ctx)?);
    }
    if let Some(o) = params.get("omit") {
        r.omit = Some(crate::repr::parse_projection(o, ctx)?);
    }
    if let Some(a) = params.get("attrs") {
        r.attrs = Some(a.split(',').map(|t| ctx.expand_key(t.trim())).collect());
    }
    r.dataset_id = params
        .get("datasetId")
        .map(|s| s.split(',').map(|d| d.trim().to_owned()).collect());
    r.page = InstancePage::parse(params)?;
    if let Some(m) = params.get("aggrMethods") {
        for method in m.split(',') {
            let method = method.trim();
            if !AGGR_METHODS.contains(&method) {
                return Err(NgsiError::BadRequestData(format!(
                    "invalid aggrMethods value {method:?} (4.5.19)"
                )));
            }
            r.aggr_methods.push(method.to_owned());
        }
        // aggrMethods implies aggregation UNLESS an explicit format says otherwise
        if !params.contains_key("format") {
            r.aggregated = true;
        }
    }
    if r.aggregated && r.aggr_methods.is_empty() {
        return Err(NgsiError::BadRequestData(
            "aggregatedValues requires aggrMethods (4.5.19)".into(),
        ));
    }
    if let Some(d) = params.get("aggrPeriodDuration") {
        let p = parse_aggr_period(d).ok_or_else(|| {
            NgsiError::BadRequestData(format!("invalid aggrPeriodDuration {d:?}"))
        })?;
        // 4.11: the value space ends where duration arithmetic does —
        // beyond ~100 years chrono::Duration::seconds is out of bounds
        // (a panic, i.e. a remote 500), so such periods are rejected.
        // Both components are bounded at ~100 years, the months in their own
        // unit since a month is not a fixed number of seconds.
        let (months, secs) = match p {
            AggrPeriod::Whole => (0, 0),
            AggrPeriod::Seconds(sc) => (0, sc),
            AggrPeriod::Months(m, sc) => (m, sc),
        };
        if months > 1200 || secs > 86_400 * 366 * 100 {
            return Err(NgsiError::BadRequestData(format!(
                "aggrPeriodDuration {d:?} is out of range"
            )));
        }
        r.aggr_period = p;
    }
    Ok(r)
}

/// The attribute-selection set for windowing: attrs= or pick=.
fn selection(r: &TRepr) -> Option<Vec<String>> {
    if let Some(a) = &r.attrs {
        return Some(a.clone());
    }
    // core-member picks (id/type/…) are presentation-only, not attr selection
    r.pick.as_ref().map(|p| {
        p.iter()
            .filter(|n| !is_meta(&n.raw))
            .map(|n| n.iri.clone())
            .collect()
    })
}

/// Aggregated representation (4.5.19): attr → `{type, <method>: [[v,start,end]]}`.
/// Aggregation datatype class per 4.5.19.1 (Tables -1, -2, -3). Booleans
/// count as numbers (1/0, table NOTE); a JSON String, a DateTime and a Date
/// share the ordered min/max column; a Time additionally supports avg.
#[derive(Clone, Copy, PartialEq, Debug)]
enum AggrClass {
    Number,
    Text,
    /// 4.6.3 DateTime or Date: ordered, and Table 4.5.19.1-2 gives it no
    /// arithmetic.
    Instant,
    TimeOfDay,
    List,
    Opaque,
    Relationship,
}

/// The 4.6.3 datatype a Property instance's value carries, in either
/// representation C.6 gives for one: a JSON-LD typed value
/// (`{"@type": "DateTime", "@value": …}`), or a string whose `valueType`
/// names the datatype and is coerced to its URI on the way in (4.5.2.2).
/// Table 4.5.19.1-2 applies to the datatype, not to the spelling.
fn value_datatype(inst: &Value) -> Option<&str> {
    fn term(s: &str) -> &str {
        s.strip_prefix(antares_jsonld::NGSI_LD_BASE).unwrap_or(s)
    }
    if let Some(vt) = inst.get("valueType").and_then(Value::as_str) {
        if inst.get("value").is_some_and(Value::is_string) {
            return Some(term(vt));
        }
    }
    inst.get("value")?
        .get("@type")
        .and_then(Value::as_str)
        .map(term)
}

/// The lexical form of a value: the string itself, or the `@value` of a
/// JSON-LD typed value.
fn lexical_of(v: &Value) -> Option<&str> {
    v.as_str()
        .or_else(|| v.get("@value").and_then(Value::as_str))
}

/// The key the ordered classes compare by. A JSON String and a Date are
/// compared as written — 4.6.3 fixes the width of every component of a Date,
/// so lexicographical order is chronological — a DateTime by its canonical
/// instant, since an optional seconds fraction is written before the `Z` it
/// follows and sorts ahead of it, and a Time by its second of the day, at a
/// fixed width so one string comparison serves all four.
fn order_key(class: AggrClass, v: &Value) -> Option<String> {
    let s = lexical_of(v)?;
    Some(match class {
        AggrClass::Instant => antares_model::dt_key(s),
        AggrClass::TimeOfDay => format!("{:013.6}", seconds_of_day(s)?),
        _ => s.to_owned(),
    })
}

fn classify_instance(inst: &Value) -> AggrClass {
    if inst.get("object").is_some() {
        return AggrClass::Relationship;
    }
    if inst.get("valueList").is_some() || inst.get("objectList").is_some() {
        return AggrClass::List;
    }
    if inst.get("vocab").is_some()
        || inst.get("languageMap").is_some()
        || inst.get("json").is_some()
    {
        // URI / JSON-object valued kinds: only counting methods apply
        return AggrClass::Opaque;
    }
    match value_datatype(inst) {
        Some("DateTime" | "Date") => return AggrClass::Instant,
        Some("Time") => return AggrClass::TimeOfDay,
        _ => {}
    }
    match inst.get("value") {
        Some(Value::Number(_)) | Some(Value::Bool(_)) => AggrClass::Number,
        Some(Value::String(_)) => AggrClass::Text,
        Some(Value::Array(_)) => AggrClass::List,
        _ => AggrClass::Opaque,
    }
}

/// Table 4.5.19.1 eligibility: which methods apply to which datatype class.
fn aggr_eligible(class: AggrClass, method: &str) -> bool {
    match method {
        "totalCount" | "distinctCount" => true,
        "min" | "max" => !matches!(class, AggrClass::Opaque | AggrClass::Relationship),
        "avg" => matches!(
            class,
            AggrClass::Number | AggrClass::List | AggrClass::TimeOfDay
        ),
        "sum" => matches!(class, AggrClass::Number | AggrClass::List),
        "stddev" | "sumsq" => matches!(class, AggrClass::Number),
        _ => false,
    }
}

/// `HH:MM:SS[.f]` → seconds of day (4.6.3 Time is UTC with optional `Z`).
fn seconds_of_day(s: &str) -> Option<f64> {
    let t = s.strip_suffix('Z').unwrap_or(s);
    let b = t.as_bytes();
    if b.len() < 8 || b.get(2) != Some(&b':') || b.get(5) != Some(&b':') {
        return None;
    }
    let h: f64 = t.get(0..2)?.parse().ok()?;
    let m: f64 = t.get(3..5)?.parse().ok()?;
    let sec: f64 = t.get(6..)?.parse().ok()?;
    (h < 24.0 && m < 60.0 && sec < 62.0).then_some(h * 3600.0 + m * 60.0 + sec)
}

/// The raw member an instance carries its data under.
fn raw_of(inst: &Value) -> Option<&Value> {
    for k in [
        "value",
        "object",
        "valueList",
        "objectList",
        "vocab",
        "languageMap",
        "json",
    ] {
        if let Some(v) = inst.get(k) {
            return Some(v);
        }
    }
    None
}

/// Numeric view of one raw value for the class (None ⇒ excluded from
/// numeric methods; List aggregates SIZES per Table 4.5.19.1-1).
fn numeric_of(class: AggrClass, v: &Value) -> Option<f64> {
    match class {
        AggrClass::Number => match v {
            Value::Bool(b) => Some(if *b { 1.0 } else { 0.0 }),
            _ => v.as_f64(),
        },
        AggrClass::List => v.as_array().map(|a| a.len() as f64),
        AggrClass::TimeOfDay => lexical_of(v).and_then(seconds_of_day),
        _ => None,
    }
}

fn render_aggregated(
    w: &Windowed,
    tq: Option<&TemporalQ>,
    r: &TRepr,
    ctx: &Context,
    timeprop: &str,
) -> Result<Map<String, Value>, NgsiError> {
    use chrono::{DateTime, Datelike, FixedOffset};
    let fmt = |d: DateTime<FixedOffset>| d.format("%Y-%m-%dT%H:%M:%SZ").to_string();
    let mut out = Map::new();
    for (k, instances) in &w.attrs {
        let mut times: Vec<(DateTime<FixedOffset>, &Value)> = Vec::new();
        let mut class: Option<AggrClass> = None;
        for inst in instances {
            let Some(t) = inst
                .get(timeprop)
                .and_then(Value::as_str)
                .and_then(|t| DateTime::parse_from_rfc3339(t).ok())
            else {
                continue;
            };
            if class.is_none() {
                class = Some(classify_instance(inst));
            }
            let Some(raw) = raw_of(inst) else { continue };
            times.push((t, raw));
        }
        if times.is_empty() {
            continue;
        }
        // Set alongside the first instance, and `times` is non-empty here.
        let Some(class) = class else { continue };
        // 5.7.4.4 p.211: "If an aggregated temporal representation is
        // requested and any of the requested Attributes is not eligible for
        // at least one of the aggregation methods specified in the request
        // parameters, then an error of type InvalidRequest shall be raised."
        for method in &r.aggr_methods {
            if !aggr_eligible(class, method) {
                return Err(NgsiError::InvalidRequest(format!(
                    "attribute {} ({class:?}-valued) is not eligible for \
                     aggregation method {method} (4.5.19.1, 5.7.4.4)",
                    ctx.compact_iri(k)
                )));
            }
        }
        // ADR-N-054 2.5 (DM-118): an `onChange` Attribute leaves out the rows
        // that repeat a value, and a left-out row is not a zero.
        let held = w.held.contains(k);
        if let Some(method) = r
            .aggr_methods
            .iter()
            .find(|m| held && matches!(m.as_str(), "sum" | "totalCount"))
        {
            return Err(NgsiError::InvalidRequest(format!(
                "aggregation-not-meaningful: {method} over {}, which records a row only \
                 when its value changes (ADR-N-054 2.5)",
                ctx.compact_iri(k)
            )));
        }
        times.sort_by_key(|(t, _)| *t);
        let Some(&(earliest, _)) = times.first() else {
            continue;
        };
        let anchor = tq
            .and_then(|tq| DateTime::parse_from_rfc3339(&tq.time_at).ok())
            .unwrap_or(earliest);
        // 4.5.19.1: "A duration of 0 second (e.g. expressed as "PT0S" or
        // "P0D") is valid and is interpreted as a duration spanning the whole
        // time range specified by the temporal query." The query names one
        // edge of that range; 4.11 leaves the other open for `before` and
        // `after`, so the data closes the open one.
        let whole = {
            let at = |s: &str| DateTime::parse_from_rfc3339(s).ok();
            let (Some(&(first, _)), Some(&(last_at, _))) = (times.first(), times.last()) else {
                continue;
            };
            let last = last_at
                .checked_add_signed(chrono::Duration::seconds(1))
                .unwrap_or(last_at);
            match tq {
                Some(q) if q.timerel == "before" => (first, at(&q.time_at).unwrap_or(last)),
                Some(q) if q.timerel == "between" => (
                    at(&q.time_at).unwrap_or(first),
                    q.end_time_at.as_deref().and_then(at).unwrap_or(last),
                ),
                Some(q) if q.timerel == "after" => (at(&q.time_at).unwrap_or(first), last),
                _ => (first, last),
            }
        };
        // the row carried in from before the window holds from its start
        if held {
            for (t, _) in &mut times {
                *t = (*t).max(whole.0);
            }
        }
        // bucket boundaries
        let bucket_of =
            |t: DateTime<FixedOffset>| -> (DateTime<FixedOffset>, DateTime<FixedOffset>) {
                match r.aggr_period {
                    AggrPeriod::Whole => whole,
                    AggrPeriod::Seconds(sc) => {
                        // checked throughout: an offset no representable
                        // date can hold puts the instant in one final
                        // open-ended bucket instead of panicking
                        let idx = t.signed_duration_since(anchor).num_seconds().div_euclid(sc);
                        let bucket = idx
                            .checked_mul(sc)
                            .and_then(chrono::Duration::try_seconds)
                            .and_then(|off| anchor.checked_add_signed(off))
                            .and_then(|start| {
                                chrono::Duration::try_seconds(sc)
                                    .and_then(|w| start.checked_add_signed(w))
                                    .map(|end| (start, end))
                            });
                        match bucket {
                            Some(b) => b,
                            None => (anchor, chrono::DateTime::<chrono::Utc>::MAX_UTC.into()),
                        }
                    }
                    AggrPeriod::Months(m, sc) => {
                        // start of the k-th period, O(1) in k. Negative k are
                        // the periods BEFORE the anchor, which is what a
                        // `before` query is made of.
                        let step = |k: i64| -> Option<DateTime<FixedOffset>> {
                            let n = k.unsigned_abs().checked_mul(u64::from(m))?;
                            let n = chrono::Months::new(u32::try_from(n).ok()?);
                            let base = if k < 0 {
                                anchor.checked_sub_months(n)?
                            } else {
                                anchor.checked_add_months(n)?
                            };
                            let off = chrono::Duration::try_seconds(k.checked_mul(sc)?)?;
                            base.checked_add_signed(off)
                        };
                        // The whole-month distance ignores the day and the time
                        // of day, and one period is at least one month, so it
                        // brackets the exact index within one step: binary-search
                        // between it and the anchor instead of walking there,
                        // which is O(log) however far the instant is.
                        let approx = i64::from(t.year())
                            .saturating_sub(i64::from(anchor.year()))
                            .saturating_mul(12)
                            .saturating_add(i64::from(t.month()))
                            .saturating_sub(i64::from(anchor.month()))
                            .div_euclid(i64::from(m));
                        let (mut lo, mut hi) = (
                            approx.min(0).saturating_sub(1),
                            approx.max(0).saturating_add(1),
                        );
                        while hi.saturating_sub(lo) > 1 {
                            let mid = lo.saturating_add(hi.saturating_sub(lo).div_euclid(2));
                            if step(mid).is_some_and(|s| s <= t) {
                                lo = mid;
                            } else {
                                hi = mid;
                            }
                        }
                        // saturate instead of panic: a huge month period or a
                        // far-future timeAt overflows chrono's range — treat the
                        // remainder as one open-ended bucket
                        match (step(lo), step(hi)) {
                            (Some(start), Some(end)) if start <= t => (start, end),
                            (Some(start), None) if start <= t => {
                                (start, chrono::DateTime::<chrono::Utc>::MAX_UTC.into())
                            }
                            _ => (anchor, chrono::DateTime::<chrono::Utc>::MAX_UTC.into()),
                        }
                    }
                }
            };
        type Bucket = (DateTime<FixedOffset>, DateTime<FixedOffset>);
        type Rows<'a> = Vec<(DateTime<FixedOffset>, &'a Value)>;
        let mut buckets: Vec<(Bucket, Rows)> = Vec::new();
        // An onChange attribute held its value through a period no row fell in: that period
        // answers the value, as `every` would have, up to the window's end, never past now,
        // and at most MAX_HELD_BUCKETS of them: a far edge and PT1S is refused, not allocated (T-4669).
        let edge = whole.1.min(chrono::Utc::now().fixed_offset());
        let mut refused = false;
        let mut fill = |buckets: &mut Vec<(Bucket, Rows)>, until: DateTime<FixedOffset>| {
            while let Some(end) = buckets.last().map(|((_, end), _)| *end) {
                let gap = bucket_of(end);
                if end >= until || end >= edge || gap.0 < end {
                    break;
                }
                if buckets.len() >= MAX_HELD_BUCKETS {
                    refused = true;
                    break;
                }
                buckets.push((gap, Vec::new()));
            }
        };
        for &(t, v) in &times {
            let b = bucket_of(t);
            if held {
                fill(&mut buckets, b.0);
            }
            match buckets.last_mut() {
                Some((bb, rows)) if bb.0 == b.0 => rows.push((t, v)),
                _ => buckets.push((b, vec![(t, v)])),
            }
        }
        if held {
            fill(&mut buckets, whole.1);
        }
        if refused {
            return Err(NgsiError::BadRequestData(format!(
                "aggrPeriodDuration splits the window of {} into more than {MAX_HELD_BUCKETS} \
                 periods; ask a longer period or a shorter window",
                ctx.compact_iri(k)
            )));
        }
        // the value the last bucket with a row ended on holds into the next one
        let carried: Vec<Option<&Value>> = buckets
            .iter()
            .scan(None, |last, (_, rows)| {
                let before = *last;
                if let Some(&(_, v)) = rows.last() {
                    *last = Some(v);
                }
                Some(before)
            })
            .collect();
        let mut attr_out = Map::new();
        // 4.5.19.0: the member is labelled "Property" for Properties and
        // "Relationship" for Relationships.
        let label = if class == AggrClass::Relationship {
            "Relationship"
        } else {
            "Property"
        };
        attr_out.insert("type".into(), Value::String(label.into()));
        for method in &r.aggr_methods {
            let rows: Vec<Value> = buckets
                .iter()
                .zip(&carried)
                .map(|(((bs, be), rows), &before)| {
                    let weighted = (held && method == "avg")
                        .then(|| time_weighted_avg(class, rows, before, *bs, (*be).min(whole.1)))
                        .flatten();
                    let val = weighted.unwrap_or_else(|| {
                        let vals: Vec<&Value> = match (rows.is_empty(), before) {
                            (true, Some(v)) => vec![v],
                            _ => rows.iter().map(|&(_, v)| v).collect(),
                        };
                        aggregate_bucket(method, class, &vals)
                    });
                    Value::Array(vec![val, Value::String(fmt(*bs)), Value::String(fmt(*be))])
                })
                .collect();
            attr_out.insert(method.clone(), Value::Array(rows));
        }
        out.insert(ctx.compact_iri(k), Value::Object(attr_out));
    }
    Ok(out)
}

/// The mean of a bucket from `start` to `end` whose `rows` (time, value) each
/// hold until the next, `before` holding until the first (ADR-N-054 2.5);
/// `None` for a class with no number or a bucket no value held for a moment.
fn time_weighted_avg(
    class: AggrClass,
    rows: &[(chrono::DateTime<chrono::FixedOffset>, &Value)],
    before: Option<&Value>,
    start: chrono::DateTime<chrono::FixedOffset>,
    end: chrono::DateTime<chrono::FixedOffset>,
) -> Option<Value> {
    if class == AggrClass::TimeOfDay {
        return None;
    }
    let held = before
        .map(|v| (start, v))
        .into_iter()
        .chain(rows.iter().copied());
    let until = rows
        .iter()
        .skip(usize::from(before.is_none()))
        .map(|&(t, _)| t)
        .chain([end]);
    let (mut weighted, mut span) = (0.0, 0.0);
    for ((from, v), to) in held.zip(until) {
        let secs = to.signed_duration_since(from).num_milliseconds() as f64 / 1000.0;
        if let (Some(x), true) = (numeric_of(class, v), secs > 0.0) {
            weighted += x * secs;
            span += secs;
        }
    }
    (span > 0.0).then(|| serde_json::json!(weighted / span))
}

/// One bucket, one method — per-class semantics from Tables 4.5.19.1-1/2/3.
/// Never emits an out-of-range float (the old fold seeded with
/// f64::INFINITY, which serde_json serializes as null).
fn aggregate_bucket(method: &str, class: AggrClass, vals: &[&Value]) -> Value {
    let nums: Vec<f64> = vals.iter().filter_map(|v| numeric_of(class, v)).collect();
    let finite = |x: f64| {
        if x.is_finite() {
            serde_json::json!(x)
        } else {
            Value::Null
        }
    };
    match method {
        "totalCount" => serde_json::json!(vals.len()),
        "distinctCount" => {
            // Relationship: "count of distinct relationship TARGETS" — an
            // object may be a URI or an array of URIs, so flatten first.
            let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();
            for v in vals {
                let items: Vec<&Value> = match (class, v) {
                    (AggrClass::Relationship, Value::Array(a)) => a.iter().collect(),
                    _ => vec![*v],
                };
                for it in items {
                    seen.insert(it.to_string());
                }
            }
            serde_json::json!(seen.len())
        }
        "min" | "max" => match class {
            // ordered classes: the first or last value in the order the
            // tables give the datatype, returned as it was written
            AggrClass::Text | AggrClass::Instant | AggrClass::TimeOfDay => {
                let mut keyed: Vec<(String, &Value)> = vals
                    .iter()
                    .filter_map(|v| order_key(class, v).map(|k| (k, *v)))
                    .collect();
                keyed.sort_by(|a, b| a.0.cmp(&b.0));
                let pick = if method == "min" {
                    keyed.first()
                } else {
                    keyed.last()
                };
                pick.map_or(Value::Null, |(_, v)| (*v).clone())
            }
            _ => {
                let it = nums.iter().copied();
                let picked = if method == "min" {
                    it.fold(None, |a: Option<f64>, v| Some(a.map_or(v, |x| x.min(v))))
                } else {
                    it.fold(None, |a: Option<f64>, v| Some(a.map_or(v, |x| x.max(v))))
                };
                picked.map_or(Value::Null, &finite)
            }
        },
        "sum" => finite(nums.iter().sum::<f64>()),
        "avg" => {
            if nums.is_empty() {
                Value::Null
            } else if class == AggrClass::TimeOfDay {
                let mean = nums.iter().sum::<f64>() / nums.len() as f64;
                let (h, m, sec) = (
                    (mean / 3600.0) as u32,
                    ((mean % 3600.0) / 60.0) as u32,
                    (mean % 60.0) as u32,
                );
                // 4.6.3: a Time is `hh:mm:ssZ`, and its JSON-LD type is what
                // tells a reader it is one — written bare it reads back as a
                // JSON String, which Table 4.5.19.1-1 gives no average.
                serde_json::json!({
                    "@type": "Time",
                    "@value": format!("{h:02}:{m:02}:{sec:02}Z"),
                })
            } else {
                finite(nums.iter().sum::<f64>() / nums.len() as f64)
            }
        }
        "stddev" => {
            if nums.is_empty() {
                Value::Null
            } else {
                let n = nums.len() as f64;
                let mean = nums.iter().sum::<f64>() / n;
                finite((nums.iter().map(|v| (v - mean).powi(2)).sum::<f64>() / n).sqrt())
            }
        }
        "sumsq" => finite(nums.iter().map(|v| v * v).sum::<f64>()),
        _ => Value::Null,
    }
}

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
async fn query_temporal_outer(
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
async fn check_temporal_query(
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
fn respond_aggregated(
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
fn type_and_attrs_match(
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
async fn member_now(
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
fn replays_state(rules: Option<&crate::policy::Filter>) -> bool {
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
fn cap_keeps_shown(
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
fn instant(inst: &Value) -> Option<&str> {
    inst.get("observedAt")
        .or_else(|| inst.get("modifiedAt"))
        .and_then(Value::as_str)
}

/// The Entity as it stood at `t`, rebuilt from its history: the frame, and
/// for each Attribute the latest instance at or before `t`, if any.
fn state_at(doc: &Value, t: &str) -> Value {
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

fn window_conditions_match(
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
fn retain_q_instances(
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
fn scope_window_retain(doc: &mut Value, sq: &str, tq: Option<&TemporalQ>, timeprop: &str) -> bool {
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
            truncate(&mut w, &timeprop, page_n.desc);
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
            page_n.step(trepr.aggregated),
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
async fn retrieve_temporal_outer(
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

async fn retrieve_temporal_inner(
    st: &AppState,
    id: &str,
    params: &HashMap<String, String>,
    headers: &HeaderMap,
    map: Option<&Value>,
) -> ApiResult<Response> {
    {
        let tenant = tenant_from(headers)?;
        check_params(
            params,
            &[
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
                "versions",
            ],
        )?;
        // T-4665, ADR-N-054 2.4: the chain of each measurement, for a caller
        // the space's Policy granted `readVersions` (the gateway says so)
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
        if versions && !crate::provenance::may_read_versions(headers, st.provenance_from.as_deref())
        {
            return Err(ApiError::Denied(
                "versions=all needs readVersions, which the space's Policy grants through the gateway"
                    .into(),
            ));
        }
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
        let page_n = trepr.page;
        antares_model::EntityId::new(id)?;
        // ADR-0023 rule 11: a condition rebuilds each instance's state from
        // the whole history, so the store prunes no instance under one; rules
        // without one read only the window (T-4246)
        let types_rule = crate::repr::type_rules(&filter);
        let replays = replays_state(types_rule.as_deref());
        // Instance pruning pushed into the store (no q=/geo on retrieve,
        // so it is always safe here); window() below stays the arbiter.
        let tf = antares_store::filter::TemporalFilter {
            range: tq
                .as_ref()
                .filter(|_| !replays)
                .map(|t| antares_store::filter::InstanceRange {
                    timerel: &t.timerel,
                    time_at: &t.time_at,
                    end_time_at: t.end_time_at.as_deref(),
                    timeproperty: &t.timeproperty,
                }),
            last_n: page_n.store_cap().filter(|_| {
                cap_keeps_shown(
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
            ..Default::default()
        };
        let timeprop = tq
            .as_ref()
            .map_or("observedAt", |t| t.timeproperty.as_str())
            .to_owned();
        let local = st.temporal.get_temporal(&tenant, id, &tf).await?;
        // 5.7.3.4: forward to matching retrieveTemporal registrations and
        // merge the remote instance data (4.5.5; auxiliary instances only
        // fill timestamps absent elsewhere)
        let mut warnings: Vec<String> = Vec::new();
        let looped = crate::federation::via_loop(
            headers,
            &crate::federation::alias_for(&st.host_alias, &tenant),
        );
        let doc = if crate::federation::active(params) && !looped {
            let fed = crate::federation::fed_retrieve_temporal(
                st,
                &tenant,
                headers,
                &ctx,
                id,
                params,
                map,
                &mut warnings,
            )
            .await?;
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
                            ))
                            .into())
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
                        merge_temporal_docs(&mut base, d, *aux, &timeprop);
                    }
                }
            }
            base
        } else {
            local.ok_or_else(|| {
                NgsiError::ResourceNotFound(format!("temporal entity {id} not found"))
            })?
        };
        // ADR-0020, ADR-0023 rules 3 and 8: an Entity outside the
        // narrowing now is the 404 of an absent one
        if !member_now(st, &tenant, &ctx, &filter, id, Some(&doc)).await? {
            return Err(
                NgsiError::ResourceNotFound(format!("temporal entity {id} not found")).into(),
            );
        }
        // ADR-0023 rule 11: the history its rules show, before anything
        // reads it; no rule for its types is the 404 of an absent Entity
        let doc = match types_rule.as_deref() {
            Some(r) => rules_per_instance(&doc, r).ok_or_else(|| {
                NgsiError::ResourceNotFound(format!("temporal entity {id} not found"))
            })?,
            None => doc,
        };
        let attrs_filter = selection(&trepr);
        // 5.7.3: attrs matching nothing ⇒ 404. A `pick` may name core members
        // instead — 5.7.3.3 admits "id", "type", "scope" or an Attribute name
        // — and then it selects no Attribute at all. 5.7.3.5 still reduces the
        // Entity to the members it names, so that empty selection is the
        // answer rather than a Temporal Evolution holding none of it.
        let core_only_pick = trepr.attrs.is_none()
            && trepr.pick.is_some()
            && attrs_filter.as_ref().is_some_and(Vec::is_empty);
        if let Some(want) = &attrs_filter {
            if !core_only_pick && !want.iter().any(|a| doc.get(a).is_some()) {
                return Err(NgsiError::ResourceNotFound(format!(
                    "temporal entity {id} has none of the requested attributes"
                ))
                .into());
            }
        }
        let mut w = window(
            &doc,
            tq.as_ref(),
            page_n,
            attrs_filter.as_ref(),
            trepr.omit.as_ref(),
            trepr.dataset_id.as_ref(),
            &timeprop,
            crate::history::held_attrs(st, &tenant, &doc),
        );
        if !trepr.aggregated {
            truncate(&mut w, &timeprop, page_n.desc);
            // after the cut: lastN counts measurements, never versions (DM-119)
            if versions {
                with_versions(&mut w, &st.temporal.versions(&tenant, id).await?);
            }
        }
        let cr = if trepr.aggregated {
            None
        } else {
            content_range(
                w.truncated,
                w.ts_min.as_deref(),
                w.ts_max.as_deref(),
                tq.as_ref(),
                page_n,
            )
        };
        let payload = present_temporal(&doc, &w, &ctx, &trepr, tq.as_ref(), &timeprop)?;
        if (trepr.pick.is_some() || trepr.omit.is_some())
            && payload.as_object().is_some_and(|o| o.is_empty())
        {
            return Err(NgsiError::ResourceNotFound(format!(
                "projection matches nothing on temporal entity {id}"
            ))
            .into());
        }
        let status = if cr.is_some() {
            StatusCode::PARTIAL_CONTENT
        } else {
            StatusCode::OK
        };
        let mut resp = respond(status, payload, &ctx, accept, &tenant);
        attach_paging(
            &mut resp,
            None,
            &interval_links(
                params,
                &format!(
                    "/ngsi-ld/v1/temporal/entities/{}",
                    crate::federation::path_segment(id)
                ),
                accept,
                page_n,
                w.more,
                page_n.step(trepr.aggregated),
                w.cut_served.zip(cut_boundary(&w, page_n.desc)),
            ),
        );
        if let Some(cr) = cr {
            if let Ok(v) = cr.parse() {
                resp.headers_mut().insert("Content-Range", v);
            }
        }
        crate::paging::attach_warnings(&mut resp, &warnings);
        filter.mark(&st.host_alias, resp.headers_mut());
        Ok(resp)
    }
}

/// 5.7.3.4 / 4.5.5: merge one remote Temporal Evolution into `base` by
/// appending instances per Attribute; auxiliary data contributes an
/// instance only when no instance with the same timeproperty value was
/// received from elsewhere.
fn merge_temporal_docs(base: &mut Value, add: &Value, aux: bool, timeprop: &str) {
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

// ---------- DELETE /temporal/entities/{id} (5.6.16) ----------

pub async fn delete_temporal(
    State(st): State<AppState>,
    Path(id): Path<String>,
    CleanParams(params): CleanParams,
    headers: HeaderMap,
) -> Response {
    let go = async {
        let tenant = tenant_from(&headers)?;
        antares_model::EntityId::new(&id)?;
        check_params(&params, &["local"])?;
        // 5.6.16.4: forward to registrations supporting the operation;
        // unsupported proxy modes are Conflict.
        let ctx = st.loader.core();
        let pre = crate::policy::temporal_pre_image(&st, &tenant, &id).await;
        gate!(
            st, &tenant, &headers, "5.6.16",
            ids: &[&id],
            types: &pre.types,
            stored: pre.stored.as_ref(),
        )
        .await?;
        let regs = match temporal_write_regs(&st, &tenant, &headers, &ctx, &params, &id).await {
            Ok(regs) => regs,
            Err(refused) => return Ok(*refused),
        };
        let deleted = st.temporal.delete(&tenant, &id).await?;
        answer_temporal_attr_write(
            &st,
            &tenant,
            &headers,
            &ctx,
            &id,
            "deleteTemporal",
            reqwest::Method::DELETE,
            "",
            None,
            regs,
            LocalWrite {
                res: deleted.then_some(Ok(())),
                found: deleted,
                missing: format!("temporal entity {id}"),
                applied: "deleted locally",
            },
        )
        .await
    };
    go.await.unwrap_or_else(|e| e.into_response())
}

// ---------- POST /temporal/entities/{id}/attrs/ (5.6.12) ----------

pub async fn add_temporal_attrs(
    State(st): State<AppState>,
    Path(id): Path<String>,
    CleanParams(params): CleanParams,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let go = async {
        let tenant = tenant_from(&headers)?;
        antares_model::EntityId::new(&id)?;
        check_params(&params, &["local"])?;
        let parsed = parse_body(&st.loader, &headers, &body, BodyKind::Standard).await?;
        let obj = parsed.object(NgsiError::BadRequestData(
            "fragment must be a JSON object".into(),
        ))?;
        let pre = crate::policy::temporal_pre_image(&st, &tenant, &id).await;
        let granted = gate!(
            st, &tenant, &headers, "5.6.12",
            ids: &[&id],
            types: &pre.types,
            stored: pre.stored.as_ref(),
        )
        .await?;
        let cond = crate::policy::Condition::of(&granted);
        // 5.6.12 input is pushed history — the 4.5.7 deleted-instance
        // representation is legal (5.5.4 temporal exception), hence
        // allow_null (mirrors 5.6.11).
        let mut expanded = expand_entity(
            obj,
            &parsed.ctx,
            ExpandOpts {
                fragment: true,
                allow_null: true,
                temporal: true,
                ..Default::default()
            },
        )?;
        // 5.6.12.4: forwarding — proxy modes without appendAttrsTemporal
        // are Conflict; supporting registrations receive the fragment and
        // the matching attributes are stripped from the local half.
        let spec = crate::registry::CsrSpec {
            ids: Some(vec![id.clone()]),
            ..Default::default()
        };
        let regs = match crate::federation::write_plan(
            &st,
            &tenant,
            &spec,
            &parsed.ctx,
            &params,
            &headers,
        )
        .await?
        {
            crate::federation::WritePlan::Answered(r) => return Ok(*r),
            crate::federation::WritePlan::Forward(regs) => regs,
        };
        crate::policy::Condition::unforwarded(cond.as_ref(), !regs.is_empty())?;
        if !regs.is_empty() {
            let mut parts = Vec::new();
            let mut fwd = Vec::new();
            for reg in &regs {
                if !reg.supports("appendAttrsTemporal") {
                    if reg.is_proxy() {
                        parts.push(crate::federation::conflict_part("appendAttrsTemporal"));
                    }
                    continue;
                }
                if let Some(frag) = crate::federation::reduce_to_scope(obj, reg, &parsed.ctx) {
                    fwd.push((reg.clone(), frag));
                }
            }
            let proxies: Vec<&crate::federation::FedReg> =
                regs.iter().filter(|r| r.is_proxy()).collect();
            let (rest, has_attrs) = crate::federation::strip_proxied(obj, &proxies, &parsed.ctx);
            if has_attrs || proxies.is_empty() {
                let mut local = expand_entity(
                    &rest,
                    &parsed.ctx,
                    ExpandOpts {
                        fragment: true,
                        allow_null: true,
                        temporal: true,
                        ..Default::default()
                    },
                )?;
                let ts = now_iso();
                stamp_temporal_instances(&mut local, &ts);
                let res = st
                    .temporal
                    .mutate(&tenant, &id, |doc| {
                        add_temporal_instances(doc, &local, &ts);
                        Ok::<(), NgsiError>(())
                    })
                    .await?;
                parts.push(match res {
                    Some(Ok(())) => crate::federation::Part {
                        status: 204,
                        detail: "added locally".into(),
                    },
                    _ => crate::federation::Part {
                        status: 404,
                        detail: format!("temporal entity {id} not found locally"),
                    },
                });
            }
            let ctx_url = crate::federation::ctx_link_url(&headers, &parsed.ctx.source);
            for (reg, frag) in fwd {
                parts.push(
                    crate::federation::forward_part(
                        &st,
                        reqwest::Method::POST,
                        format!(
                            "{}/ngsi-ld/v1/temporal/entities/{}/attrs",
                            reg.endpoint,
                            crate::federation::path_segment(&id)
                        ),
                        &[],
                        &headers,
                        &tenant,
                        &reg,
                        &ctx_url,
                        Some(frag),
                    )
                    .await,
                );
            }
            return Ok(crate::federation::combine(
                parts,
                no_content(&tenant),
                &tenant,
            ));
        }
        let ts = now_iso();
        stamp_temporal_instances(&mut expanded, &ts);
        let res = st
            .temporal
            .mutate(&tenant, &id, |doc| {
                crate::policy::Condition::pre_evolution(cond.as_ref(), &id, doc)?;
                add_temporal_instances(doc, &expanded, &ts);
                Ok::<(), NgsiError>(())
            })
            .await?;
        match res {
            None => {
                Err(NgsiError::ResourceNotFound(format!("temporal entity {id} not found")).into())
            }
            Some(Err(e)) => Err(ApiError::from(e)),
            Some(Ok(())) => Ok(no_content(&tenant)),
        }
    };
    go.await.unwrap_or_else(|e| e.into_response())
}

/// 5.6.12.4: append the fragment's Attribute instances to the Temporal
/// Evolution (instances accumulate — history is never overwritten here).
fn add_temporal_instances(doc: &mut Value, expanded: &Value, ts: &str) {
    let Some(target) = doc.as_object_mut() else {
        return;
    };
    for (k, v) in expanded.as_object().into_iter().flatten() {
        if is_meta(k) {
            continue;
        }
        let incoming = v.as_array().cloned().unwrap_or_default();
        match target.get_mut(k).and_then(Value::as_array_mut) {
            Some(cur) => cur.extend(incoming),
            None => {
                target.insert(k.clone(), Value::Array(incoming));
            }
        }
    }
    target.insert("modifiedAt".into(), Value::String(ts.to_owned()));
}

// ---------- DELETE /temporal/entities/{id}/attrs/{attrId} (5.6.13) ----------

pub async fn delete_temporal_attr(
    State(st): State<AppState>,
    Path((id, attr)): Path<(String, String)>,
    CleanParams(params): CleanParams,
    headers: HeaderMap,
) -> Response {
    let go = async {
        let tenant = tenant_from(&headers)?;
        antares_model::EntityId::new(&id)?;
        // 5.6.13.4: "If the target Attribute name is not a valid name, then an
        // error of type BadRequestData shall be raised." The shared guard is
        // the one that also refuses dot segments — the name is interpolated
        // into the forwarded request path, where `..` addresses the peer's
        // Temporal Evolution resource instead of its attribute.
        antares_model::check_attr_name(&attr)?;
        check_params(&params, &["datasetId", "deleteAll", "local"])?;
        let ctx = request_context(&st.loader, &headers).await?;
        let pre = crate::policy::temporal_pre_image(&st, &tenant, &id).await;
        let granted = gate!(
            st, &tenant, &headers, "5.6.13",
            ids: &[&id],
            types: &pre.types,
            stored: pre.stored.as_ref(),
        )
        .await?;
        let cond = crate::policy::Condition::of(&granted);
        let attr_iri = antares_jsonld::expand_attr_name(&attr, &ctx)?;
        let delete_all = params.get("deleteAll").map(String::as_str) == Some("true");
        let want_ds = crate::repr::target_dataset_id(&params).map(String::from);
        let regs = match temporal_write_regs(&st, &tenant, &headers, &ctx, &params, &id).await {
            Ok(regs) => regs,
            Err(refused) => return Ok(*refused),
        };
        crate::policy::Condition::unforwarded(cond.as_ref(), !regs.is_empty())?;
        let mut found = false;
        let ts = now_iso();
        let res = st
            .temporal
            .mutate(&tenant, &id, |doc| {
                crate::policy::Condition::pre_evolution(cond.as_ref(), &id, doc)?;
                let target = antares_store::stored_object(doc)?;
                if delete_all
                    || (want_ds.is_none()
                        && !target
                            .get(&attr_iri)
                            .and_then(Value::as_array)
                            .is_some_and(|a| a.iter().any(|i| i.get("datasetId").is_some())))
                {
                    // deleteAll, or single-instance-set attribute: drop it whole
                    if target.remove(&attr_iri).is_some() {
                        found = true;
                    }
                } else if let Some(arr) = target.get_mut(&attr_iri).and_then(Value::as_array_mut) {
                    // 5.6.13: only the matching datasetId instance set is deleted
                    let before = arr.len();
                    arr.retain(|i| {
                        i.get("datasetId").and_then(Value::as_str) != want_ds.as_deref()
                    });
                    found = arr.len() != before;
                    if arr.is_empty() {
                        target.remove(&attr_iri);
                    }
                }
                if found {
                    target.insert("modifiedAt".into(), Value::String(ts.clone()));
                }
                Ok::<(), NgsiError>(())
            })
            .await?;
        answer_temporal_attr_write(
            &st,
            &tenant,
            &headers,
            &ctx,
            &id,
            "deleteAttrsTemporal",
            reqwest::Method::DELETE,
            &format!("/attrs/{}", crate::federation::path_segment(&attr)),
            None,
            regs,
            LocalWrite {
                res,
                found,
                missing: format!("attribute {attr}"),
                applied: "deleted locally",
            },
        )
        .await
    };
    go.await.unwrap_or_else(|e| e.into_response())
}

/// 5.6.13.4-5.6.15.4 shared forwarding: proxy registrations without the
/// operation's support are an error of type Conflict and are never
/// contacted; supporting registrations receive the forwarded request. None
/// = no matching registrations (the operation stays purely local).
/// `path_suffix` arrives with its client-controlled segments already
/// percent-encoded (RFC 3986 clause 3.3); the entity id is encoded here.
/// What the local `mutate` did, in the words the answer needs. 5.6.13,
/// 5.6.14, 5.6.15 and 5.6.16 all answer the same three ways: 204 when the
/// target was there, ResourceNotFound naming the Entity when the Entity was
/// not, and ResourceNotFound naming the Attribute or the instance when the
/// Entity was there but the target inside it was not.
struct LocalWrite {
    /// `None` when the Temporal Evolution itself is absent.
    res: Option<Result<(), NgsiError>>,
    /// whether the operation's target inside it was there.
    found: bool,
    /// what a 404 names once the Entity itself was found.
    missing: String,
    /// what the 204 Part reports was done here.
    applied: &'static str,
}

/// 5.6.13 / 5.6.14 / 5.6.15 / 5.6.16 answer: the local result becomes one
/// 4.3.6 Part, every registration supporting `op` (Table 4.20-1) contributes
/// another, and 4.3.6.4 combines them into the one status the client sees.
/// With no registration to forward to — the common case — the local result
/// is the whole answer. The four operations differ in what they change and
/// in what they forward, never in how the two halves are answered together.
/// The registrations a 5.6.13/5.6.14/5.6.15/5.6.16 write forwards to, with
/// 6.3.17/6.3.18 loop handling already applied. The check belongs here,
/// ahead of the local write: 508 Loop Detected is an error status, so the
/// request it answers has to leave the Temporal Evolution as it found it.
/// The refusal travels boxed — a whole `Response` in an `Err` makes every
/// `Ok` of this function carry its width.
async fn temporal_write_regs(
    st: &AppState,
    tenant: &antares_model::TenantId,
    headers: &HeaderMap,
    ctx: &antares_jsonld::Context,
    params: &HashMap<String, String>,
    id: &str,
) -> Result<Vec<crate::federation::FedReg>, Box<Response>> {
    let spec = crate::registry::CsrSpec {
        ids: Some(vec![id.to_owned()]),
        ..Default::default()
    };
    match crate::federation::write_plan(st, tenant, &spec, ctx, params, headers)
        .await
        .map_err(|e| Box::new(crate::negotiate::ApiError::from(e).into_response()))?
    {
        crate::federation::WritePlan::Answered(refused) => Err(refused),
        crate::federation::WritePlan::Forward(regs) => Ok(regs),
    }
}

#[allow(clippy::too_many_arguments)] // mirrors the wire: one param per forwarded request part
async fn answer_temporal_attr_write(
    st: &AppState,
    tenant: &antares_model::TenantId,
    headers: &HeaderMap,
    ctx: &antares_jsonld::Context,
    id: &str,
    op: &str,
    method: reqwest::Method,
    path_suffix: &str,
    body: Option<Value>,
    regs: Vec<crate::federation::FedReg>,
    local: LocalWrite,
) -> ApiResult<Response> {
    if regs.is_empty() {
        return match local.res {
            None => {
                Err(NgsiError::ResourceNotFound(format!("temporal entity {id} not found")).into())
            }
            Some(Err(e)) => Err(ApiError::from(e)),
            Some(Ok(())) if local.found => Ok(no_content(tenant)),
            Some(Ok(())) => {
                Err(NgsiError::ResourceNotFound(format!("{} not found", local.missing)).into())
            }
        };
    }
    let local_part = match &local.res {
        None => crate::federation::Part {
            status: 404,
            detail: format!("temporal entity {id} not found locally"),
        },
        Some(_) if local.found => crate::federation::Part {
            status: 204,
            detail: local.applied.into(),
        },
        Some(_) => crate::federation::Part {
            status: 404,
            detail: format!("{} not found locally", local.missing),
        },
    };
    let mut parts = vec![local_part];
    let ctx_url = crate::federation::ctx_link_url(headers, &ctx.source);
    for reg in &regs {
        if !reg.supports(op) {
            if reg.is_proxy() {
                parts.push(crate::federation::conflict_part(op));
            }
            continue;
        }
        parts.push(
            crate::federation::forward_part(
                st,
                method.clone(),
                format!(
                    "{}/ngsi-ld/v1/temporal/entities/{}{path_suffix}",
                    reg.endpoint,
                    crate::federation::path_segment(id)
                ),
                &[],
                headers,
                tenant,
                reg,
                &ctx_url,
                body.clone(),
            )
            .await,
        );
    }
    Ok(crate::federation::combine(
        parts,
        no_content(tenant),
        tenant,
    ))
}

// ---------- PATCH/DELETE .../attrs/{attrId}/{instanceId} (5.6.14/5.6.15) ----------

pub async fn modify_temporal_instance(
    State(st): State<AppState>,
    Path((id, attr, instance_id)): Path<(String, String, String)>,
    CleanParams(params): CleanParams,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let go = async {
        let tenant = tenant_from(&headers)?;
        // Empty attr segment ⇒ the URI names no resource with a PATCH method
        // (suite 016_02_06 asserts 405 here, vs 400 on the DELETE sibling).
        if attr.is_empty() {
            return Err(ApiError::Bare(StatusCode::METHOD_NOT_ALLOWED));
        }
        antares_model::EntityId::new(&id)?;
        antares_model::check_attr_name(&attr)?;
        antares_model::EntityId::new(&instance_id)
            .map_err(|_| NgsiError::BadRequestData("invalid instance id".into()))?;
        check_params(&params, &["local"])?;
        let pre = crate::policy::temporal_pre_image(&st, &tenant, &id).await;
        let granted = gate!(
            st, &tenant, &headers, "5.6.14",
            ids: &[&id],
            types: &pre.types,
            stored: pre.stored.as_ref(),
        )
        .await?;
        let cond = crate::policy::Condition::of(&granted);
        let parsed = parse_body(&st.loader, &headers, &body, BodyKind::MergePatch).await?;
        let obj = parsed.object(NgsiError::BadRequestData(
            "fragment must be a JSON object".into(),
        ))?;
        let attr_iri = antares_jsonld::expand_attr_name(&attr, &parsed.ctx)?;
        let mut frag = obj.clone();
        frag.remove("@context");
        // 5.6.14.4 takes "the Attribute instance in the EntityTemporal
        // Fragment": one keyed by the Attribute name. A bare instance (no
        // such key, ETSI 016_01) is taken as that instance itself.
        let named = (!frag.contains_key("type")).then_some(()).and_then(|()| {
            frag.iter().find_map(|(k, v)| {
                let is_attr = k == &attr
                    || antares_jsonld::expand_attr_name(k, &parsed.ctx)
                        .ok()
                        .as_ref()
                        == Some(&attr_iri);
                is_attr.then(|| v.clone())
            })
        });
        let frag =
            match named {
                Some(Value::Array(mut one)) if one.len() == 1 => one.remove(0),
                Some(Value::Object(o)) => Value::Object(o),
                Some(_) => return Err(NgsiError::BadRequestData(
                    "the EntityTemporal Fragment holds one instance of the Attribute (5.6.14.4)"
                        .into(),
                )
                .into()),
                None => Value::Object(frag),
            };
        let mut wrapper = Map::new();
        wrapper.insert(attr.clone(), frag);
        let expanded = expand_entity(
            &wrapper,
            &parsed.ctx,
            ExpandOpts {
                fragment: true,
                allow_null: false,
                temporal: true,
                ..Default::default()
            },
        )?;
        let frag_inst = expanded
            .get(&attr_iri)
            .and_then(Value::as_array)
            .and_then(|a| a.first())
            .cloned()
            .ok_or_else(|| NgsiError::BadRequestData("invalid instance fragment".into()))?;
        let regs =
            match temporal_write_regs(&st, &tenant, &headers, &parsed.ctx, &params, &id).await {
                Ok(regs) => regs,
                Err(refused) => return Ok(*refused),
            };
        crate::policy::Condition::unforwarded(cond.as_ref(), !regs.is_empty())?;
        let ts = now_iso();
        let mut found = false;
        let res = st
            .temporal
            .mutate(&tenant, &id, |doc| {
                crate::policy::Condition::pre_evolution(cond.as_ref(), &id, doc)?;
                let target = antares_store::stored_object(doc)?;
                if let Some(arr) = target.get_mut(&attr_iri).and_then(Value::as_array_mut) {
                    if let Some(inst) = arr.iter_mut().find(|i| {
                        i.get("instanceId").and_then(Value::as_str) == Some(instance_id.as_str())
                    }) {
                        found = true;
                        // 5.6.14.4: "Replace the target Attribute instance
                        // identified by the instanceId with the Attribute instance
                        // in the EntityTemporal Fragment. The createdAt property
                        // of the concerned instance shall remain unchanged, but
                        // the modifiedAt property shall be set to the timestamp
                        // corresponding to this modification." A replace, so a
                        // member only the stored instance carries does not survive
                        // it; the instance keeps the identity it is addressed by.
                        let t = antares_store::stored_object(inst)?;
                        let kept: Vec<(String, Value)> = ["createdAt", "instanceId"]
                            .iter()
                            .filter_map(|k| t.get(*k).map(|v| ((*k).to_owned(), v.clone())))
                            .collect();
                        t.clear();
                        t.extend(kept);
                        for (k, v) in antares_jsonld::expanded_object(&frag_inst)? {
                            if matches!(k.as_str(), "createdAt" | "instanceId") {
                                continue;
                            }
                            t.insert(k.clone(), v.clone());
                        }
                        t.insert("modifiedAt".into(), Value::String(ts.clone()));
                    }
                }
                Ok::<(), NgsiError>(())
            })
            .await?;
        answer_temporal_attr_write(
            &st,
            &tenant,
            &headers,
            &parsed.ctx,
            &id,
            "updateAttrInstanceTemporal",
            reqwest::Method::PATCH,
            &format!(
                "/attrs/{}/{}",
                crate::federation::path_segment(&attr),
                crate::federation::path_segment(&instance_id)
            ),
            Some(parsed.value.clone()),
            regs,
            LocalWrite {
                res,
                found,
                missing: format!("instance {instance_id}"),
                applied: "applied locally",
            },
        )
        .await
    };
    go.await.unwrap_or_else(|e| e.into_response())
}

pub async fn delete_temporal_instance(
    State(st): State<AppState>,
    Path((id, attr, instance_id)): Path<(String, String, String)>,
    CleanParams(params): CleanParams,
    headers: HeaderMap,
) -> Response {
    let go = async {
        let tenant = tenant_from(&headers)?;
        antares_model::EntityId::new(&id)?;
        antares_model::check_attr_name(&attr)?;
        antares_model::EntityId::new(&instance_id)
            .map_err(|_| NgsiError::BadRequestData("invalid instance id".into()))?;
        check_params(&params, &["local"])?;
        let ctx = request_context(&st.loader, &headers).await?;
        let pre = crate::policy::temporal_pre_image(&st, &tenant, &id).await;
        let granted = gate!(
            st, &tenant, &headers, "5.6.15",
            ids: &[&id],
            types: &pre.types,
            stored: pre.stored.as_ref(),
        )
        .await?;
        let cond = crate::policy::Condition::of(&granted);
        let attr_iri = antares_jsonld::expand_attr_name(&attr, &ctx)?;
        let regs = match temporal_write_regs(&st, &tenant, &headers, &ctx, &params, &id).await {
            Ok(regs) => regs,
            Err(refused) => return Ok(*refused),
        };
        crate::policy::Condition::unforwarded(cond.as_ref(), !regs.is_empty())?;
        let mut found = false;
        let ts = now_iso();
        let res = st
            .temporal
            .mutate(&tenant, &id, |doc| {
                crate::policy::Condition::pre_evolution(cond.as_ref(), &id, doc)?;
                let target = antares_store::stored_object(doc)?;
                if let Some(arr) = target.get_mut(&attr_iri).and_then(Value::as_array_mut) {
                    let before = arr.len();
                    arr.retain(|i| {
                        i.get("instanceId").and_then(Value::as_str) != Some(instance_id.as_str())
                    });
                    found = arr.len() != before;
                    if arr.is_empty() {
                        target.remove(&attr_iri);
                    }
                }
                if found {
                    target.insert("modifiedAt".into(), Value::String(ts.clone()));
                }
                Ok::<(), NgsiError>(())
            })
            .await?;
        answer_temporal_attr_write(
            &st,
            &tenant,
            &headers,
            &ctx,
            &id,
            "deleteAttrInstanceTemporal",
            reqwest::Method::DELETE,
            &format!(
                "/attrs/{}/{}",
                crate::federation::path_segment(&attr),
                crate::federation::path_segment(&instance_id)
            ),
            None,
            regs,
            LocalWrite {
                res,
                found,
                missing: format!("instance {instance_id}"),
                applied: "applied locally",
            },
        )
        .await
    };
    go.await.unwrap_or_else(|e| e.into_response())
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
        let parsed = parse_body(&st.loader, &headers, &body, BodyKind::Standard).await?;
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

#[cfg(test)]
mod clause_4_11 {
    use super::*;
    use serde_json::json;

    /// 5.7.3.4 / 4.5.5.3: instances with the same datasetId (or both default)
    /// AND the same timeproperty value are CONFLICTING instances of one slot
    /// — the merge resolves them to one (most recent modifiedAt wins), never
    /// serves both. Regression: the same instance held by two federated
    /// brokers came back twice (IOP_EXT_TMP_02_05).
    #[tokio::test]
    async fn merge_resolves_same_slot_instances_to_one() {
        let mut base = json!({"id": "urn:e", "type": "T", "speed": [
            {"type": "Property", "value": 10, "observedAt": "2026-05-01T00:00:00Z",
             "modifiedAt": "2026-05-01T00:00:00Z"},
        ]});
        let add = json!({"id": "urn:e", "type": "T", "speed": [
            {"type": "Property", "value": 11, "observedAt": "2026-05-01T00:00:00Z",
             "modifiedAt": "2026-06-01T00:00:00Z"},
            {"type": "Property", "value": 20, "observedAt": "2026-05-02T00:00:00Z",
             "modifiedAt": "2026-05-02T00:00:00Z"},
            {"type": "Property", "value": 30, "observedAt": "2026-05-01T00:00:00Z",
             "datasetId": "urn:d:1", "modifiedAt": "2026-05-01T00:00:00Z"},
        ]});
        merge_temporal_docs(&mut base, &add, false, "observedAt");
        let speed = base["speed"].as_array().expect("array");
        // default-instance duplicate collapsed (newer modifiedAt won),
        // the other timestamp appended, the datasetId instance is its own slot
        assert_eq!(speed.len(), 3, "{speed:?}");
        let default_slot: Vec<&Value> = speed
            .iter()
            .filter(|i| i.get("datasetId").is_none() && i["observedAt"] == "2026-05-01T00:00:00Z")
            .collect();
        assert_eq!(default_slot.len(), 1);
        assert_eq!(default_slot[0]["value"], 11, "newer modifiedAt wins");
    }

    /// 4.3.6.2: "An auxiliary Context Source Registration never overrides
    /// data held directly within a Context Broker. […] Context data from
    /// auxiliary context sources is only included if it is supplementary to
    /// the context data otherwise available to the Context Broker." On a
    /// Temporal Evolution the unit is the instance, so an auxiliary instance
    /// enters only where no other source supplied that timeproperty value.
    #[tokio::test]
    async fn an_auxiliary_instance_supplements_but_never_overrides() {
        let mut base = json!({"id": "urn:e", "type": "T", "speed": [
            {"type": "Property", "value": 10, "observedAt": "2026-05-01T00:00:00Z",
             "modifiedAt": "2026-05-01T00:00:00Z"},
        ]});
        let add = json!({"id": "urn:e", "type": "T", "speed": [
            // same slot as the local instance, and newer — still refused
            {"type": "Property", "value": 99, "observedAt": "2026-05-01T00:00:00Z",
             "modifiedAt": "2026-07-01T00:00:00Z"},
            // a timestamp nobody else supplied: supplementary, so included
            {"type": "Property", "value": 20, "observedAt": "2026-05-02T00:00:00Z"},
        ]});
        merge_temporal_docs(&mut base, &add, true, "observedAt");
        let speed = base["speed"].as_array().expect("array");
        assert_eq!(speed.len(), 2, "{speed:?}");
        assert!(
            !Value::Array(speed.clone()).to_string().contains("99"),
            "an auxiliary instance may not override an occupied slot: {speed:?}"
        );
        assert_eq!(speed[0]["value"], 10);
        assert_eq!(speed[1]["value"], 20);
    }

    /// 4.6.3 leaves the seconds fraction optional, so two Context Sources
    /// holding one instance may spell its timeproperty differently. The slot
    /// of 4.5.5.3 is the INSTANT, not the spelling: a byte comparison treats
    /// the two as separate slots and serves the same instance twice — the
    /// IOP_EXT_TMP_02_05 duplicate, reached through a different door.
    #[tokio::test]
    async fn one_instant_spelled_two_ways_is_still_one_slot() {
        let mut base = json!({"id": "urn:e", "type": "T", "speed": [
            {"type": "Property", "value": 10, "observedAt": "2026-05-01T00:00:00Z",
             "modifiedAt": "2026-05-01T00:00:00Z"},
        ]});
        let add = json!({"id": "urn:e", "type": "T", "speed": [
            {"type": "Property", "value": 11, "observedAt": "2026-05-01T00:00:00.000Z",
             "modifiedAt": "2026-06-01T00:00:00Z"},
        ]});
        merge_temporal_docs(&mut base, &add, false, "observedAt");
        let speed = base["speed"].as_array().expect("array");
        assert_eq!(speed.len(), 1, "one instant is one slot: {speed:?}");
        assert_eq!(speed[0]["value"], 11, "newer modifiedAt wins");
    }

    /// The winner of a conflicting slot is "the most recent modifiedAt", and
    /// which of two `modifiedAt` values is more recent is a comparison of
    /// instants for the same reason. A remote instance stamped
    /// `…:00.500Z` is LATER than a local one stamped `…:00Z`, though its
    /// bytes sort earlier.
    #[tokio::test]
    async fn the_more_recent_modified_at_wins_across_fraction_spellings() {
        let mut base = json!({"id": "urn:e", "type": "T", "speed": [
            {"type": "Property", "value": 10, "observedAt": "2026-05-01T00:00:00Z",
             "modifiedAt": "2026-05-01T09:00:00Z"},
        ]});
        let add = json!({"id": "urn:e", "type": "T", "speed": [
            {"type": "Property", "value": 11, "observedAt": "2026-05-01T00:00:00Z",
             "modifiedAt": "2026-05-01T09:00:00.500Z"},
        ]});
        merge_temporal_docs(&mut base, &add, false, "observedAt");
        let speed = base["speed"].as_array().expect("array");
        assert_eq!(speed.len(), 1, "{speed:?}");
        assert_eq!(speed[0]["value"], 11, "the later instant wins: {speed:?}");
    }

    /// 4.3.6.2 auxiliary supplementation is decided on the same slot, so an
    /// auxiliary instance that respells an occupied instant is still refused.
    #[tokio::test]
    async fn an_auxiliary_respelling_of_an_occupied_slot_is_refused() {
        let mut base = json!({"id": "urn:e", "type": "T", "speed": [
            {"type": "Property", "value": 10, "observedAt": "2026-05-01T00:00:00Z"},
        ]});
        let add = json!({"id": "urn:e", "type": "T", "speed": [
            {"type": "Property", "value": 99, "observedAt": "2026-05-01T00:00:00.000Z"},
        ]});
        merge_temporal_docs(&mut base, &add, true, "observedAt");
        let speed = base["speed"].as_array().expect("array");
        assert_eq!(speed.len(), 1, "{speed:?}");
        assert_eq!(speed[0]["value"], 10);
    }
}

#[cfg(test)]
mod clause_6_3_10 {
    use super::*;
    use serde_json::json;

    /// One attribute with `n` instances, one per minute from 00:00.
    fn evolution(n: usize) -> Value {
        let speed: Vec<Value> = (0..n)
            .map(|i| json!({"type": "Property", "value": i, "observedAt": at(i)}))
            .collect();
        json!({"id": "urn:ngsi-ld:Vehicle:1", "type": "Vehicle", "speed": speed})
    }

    fn at(i: usize) -> String {
        format!("2020-01-01T00:{i:02}:00Z")
    }

    fn tq(timerel: &str, time_at: &str) -> TemporalQ {
        let mut p = HashMap::new();
        p.insert("timerel".to_owned(), timerel.to_owned());
        p.insert("timeAt".to_owned(), time_at.to_owned());
        TemporalQ::from_params(&p, true).unwrap().unwrap()
    }

    async fn windowed(doc: &Value, tq: Option<&TemporalQ>, last_n: Option<usize>) -> Windowed {
        let mut w = window(
            doc,
            tq,
            page(last_n),
            None,
            None,
            None,
            "observedAt",
            Default::default(),
        );
        truncate(&mut w, "observedAt", last_n.is_some());
        w
    }

    /// The page of a request naming lastN, or neither firstN nor lastN.
    fn page(last_n: Option<usize>) -> InstancePage {
        InstancePage {
            desc: last_n.is_some(),
            n: last_n,
            offset: 0,
        }
    }

    fn observed(w: &Windowed) -> Vec<&str> {
        w.attrs["speed"]
            .iter()
            .map(|i| i["observedAt"].as_str().unwrap())
            .collect()
    }

    /// 6.3.10: a temporal retrieval the broker cannot serve in full is
    /// answered with 206 and a Content-Range. The body must then BE the
    /// partial representation the header describes — a window wide enough to
    /// select more instances than the broker serves at once is cut to the
    /// ceiling, oldest first, and the advertised range ends at the last
    /// instance returned.
    #[tokio::test]
    async fn wide_window_is_cut_to_the_ceiling_and_the_range_matches_the_body() {
        let doc = evolution(20);
        let q = tq("after", "2019-01-01T00:00:00Z");
        let w = windowed(&doc, Some(&q), None).await;
        assert_eq!(w.attrs["speed"].len(), TEMPORAL_INSTANCE_LIMIT);
        assert!(w.truncated);
        let got = observed(&w);
        assert_eq!(got[0], at(0));
        assert_eq!(
            got[TEMPORAL_INSTANCE_LIMIT - 1],
            at(TEMPORAL_INSTANCE_LIMIT - 1)
        );
        assert!(
            !got.contains(&at(19).as_str()),
            "instances beyond the ceiling must not be served: {got:?}"
        );
        // 5.7.3.4 + 6.3.10: start is the requested lower bound, end the last
        // instance in the body, so the header cannot promise more than it sent
        assert_eq!(
            content_range(
                w.truncated,
                w.ts_min.as_deref(),
                w.ts_max.as_deref(),
                Some(&q),
                page(None)
            ),
            Some(format!(
                "date-time 2019-01-01T00:00:00Z-{}/*",
                at(TEMPORAL_INSTANCE_LIMIT - 1)
            ))
        );
    }

    /// Two attributes, `speed` minutes 0..n and `heading` minutes 5..m.
    fn evolution2(n: usize, m: usize) -> Value {
        let mut doc = evolution(n);
        doc["heading"] = (5..m)
            .map(|i| json!({"type": "Property", "value": i, "observedAt": at(i)}))
            .collect();
        doc
    }

    fn last_observed(w: &Windowed, attr: &str) -> Option<String> {
        w.attrs[attr]
            .iter()
            .filter_map(|i| i["observedAt"].as_str().map(str::to_owned))
            .max()
    }

    /// T-3613: the time continuation keeps the window's other end, and
    /// moves the end the page reached in its order.
    #[test]
    fn a_page_cut_to_nothing_resumes_past_its_boundary() {
        let p = |kv: &[(&str, &str)]| -> HashMap<String, String> {
            kv.iter()
                .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
                .collect()
        };
        let (a, b, e) = (
            "2020-01-01T00:00:00Z",
            "2020-01-01T00:08:00Z",
            "2020-01-02T00:00:00Z",
        );
        let between = |from: &str, to: &str| {
            vec![
                ("timerel", "between".to_owned()),
                ("timeAt", from.to_owned()),
                ("endTimeAt", to.to_owned()),
            ]
        };
        let one =
            |rel: &str, at: &str| vec![("timerel", rel.to_owned()), ("timeAt", at.to_owned())];
        let window = p(&[("timerel", "between"), ("timeAt", a), ("endTimeAt", e)]);
        assert_eq!(resume_after(&window, false, b), between(b, e));
        assert_eq!(resume_after(&window, true, b), between(a, b));
        let before = p(&[("timerel", "before"), ("timeAt", e)]);
        assert_eq!(resume_after(&before, false, b), between(b, e));
        assert_eq!(resume_after(&before, true, b), one("before", b));
        let after = p(&[("timerel", "after"), ("timeAt", a)]);
        assert_eq!(resume_after(&after, false, b), one("after", b));
        assert_eq!(resume_after(&after, true, b), between(a, b));
        assert_eq!(resume_after(&p(&[]), false, b), one("after", b));
        assert_eq!(resume_after(&p(&[]), true, b), one("before", b));
    }

    /// 6.3.10: the partial content IS the representation the Content-Range
    /// describes — so the cut is one time boundary for the whole entity.
    /// With `speed` over-full first, `heading` is trimmed to the same last
    /// instant, and nothing of either attribute lies past the advertised
    /// range-end (a client continuing from it misses no instance).
    #[tokio::test]
    async fn the_cut_is_one_time_boundary_across_attributes() {
        let w = windowed(&evolution2(21, 31), None, None).await;
        assert!(w.truncated);
        let end = at(TEMPORAL_INSTANCE_LIMIT - 1);
        assert_eq!(w.attrs["speed"].len(), TEMPORAL_INSTANCE_LIMIT);
        assert_eq!(last_observed(&w, "speed").as_deref(), Some(end.as_str()));
        assert_eq!(
            last_observed(&w, "heading").as_deref(),
            Some(end.as_str()),
            "heading must stop at speed's boundary, not at its own ninth instance"
        );
        assert_eq!(w.attrs["heading"].len(), TEMPORAL_INSTANCE_LIMIT - 5);
        assert_eq!(w.ts_max.as_deref(), Some(end.as_str()));
        for inst in w.attrs["heading"].iter().chain(w.attrs["speed"].iter()) {
            assert!(
                inst["observedAt"].as_str().expect("t") <= end.as_str(),
                "no instance may lie past the advertised range-end: {inst}"
            );
        }
        // backwards (lastN): the boundary is the LATEST ninth instant, so the
        // page covers [heading's ninth-newest, newest] and `speed`, which ends
        // before that, is empty on this page rather than incoherently present
        let w = windowed(&evolution2(21, 31), None, Some(20)).await;
        assert!(w.truncated);
        assert_eq!(w.attrs["heading"].len(), TEMPORAL_INSTANCE_LIMIT);
        assert_eq!(w.attrs["heading"][0]["observedAt"], json!(at(30)));
        assert_eq!(
            w.ts_min.as_deref(),
            Some(at(30 - TEMPORAL_INSTANCE_LIMIT + 1).as_str())
        );
        assert!(
            w.attrs["speed"].is_empty(),
            "speed lies entirely before the page boundary: {:?}",
            w.attrs["speed"]
        );
    }

    /// 5.7.3.4/5.7.4.4 lastN "shall be limited to the specified number of
    /// instances" — an upper limit, not an entitlement: a lastN above the
    /// broker ceiling is served up to the ceiling (newest first) and the
    /// answer is the partial one. The Content-Range size stays the requested
    /// lastN, its start-end pair the instants actually returned.
    #[tokio::test]
    async fn last_n_above_the_ceiling_is_clamped_to_it() {
        let doc = evolution(20);
        let w = windowed(&doc, None, Some(20)).await;
        assert_eq!(w.attrs["speed"].len(), TEMPORAL_INSTANCE_LIMIT);
        assert!(w.truncated);
        let got = observed(&w);
        assert_eq!(got[0], at(19), "lastN delivers newest first");
        assert_eq!(
            got[TEMPORAL_INSTANCE_LIMIT - 1],
            at(20 - TEMPORAL_INSTANCE_LIMIT)
        );
        assert_eq!(
            content_range(
                w.truncated,
                w.ts_min.as_deref(),
                w.ts_max.as_deref(),
                None,
                page(Some(20))
            ),
            Some(format!(
                "date-time {}-{}/20",
                at(19),
                at(20 - TEMPORAL_INSTANCE_LIMIT)
            ))
        );
    }

    /// 5.7.3.4: temporalQ is optional on retrieval, so a request naming no
    /// window at all asks for the whole Temporal Evolution. It is still
    /// bounded by the same ceiling, and still answered as partial.
    #[tokio::test]
    async fn a_request_with_no_window_is_capped_by_default() {
        let doc = evolution(20);
        let w = windowed(&doc, None, None).await;
        assert_eq!(w.attrs["speed"].len(), TEMPORAL_INSTANCE_LIMIT);
        assert!(w.truncated);
        assert_eq!(
            w.ts_max.as_deref(),
            Some(at(TEMPORAL_INSTANCE_LIMIT - 1).as_str())
        );
        assert_eq!(
            content_range(
                w.truncated,
                w.ts_min.as_deref(),
                w.ts_max.as_deref(),
                None,
                page(None)
            ),
            Some(format!(
                "date-time {}-{}/*",
                at(0),
                at(TEMPORAL_INSTANCE_LIMIT - 1)
            ))
        );
    }

    /// 6.3.10: partial content is conditional on truncation — a result the
    /// broker serves in full is a plain 200 with no Content-Range.
    #[tokio::test]
    async fn a_complete_result_is_not_partial_content() {
        let doc = evolution(TEMPORAL_INSTANCE_LIMIT);
        let w = windowed(&doc, None, None).await;
        assert_eq!(w.attrs["speed"].len(), TEMPORAL_INSTANCE_LIMIT);
        assert!(!w.truncated);
        assert_eq!(
            content_range(
                w.truncated,
                w.ts_min.as_deref(),
                w.ts_max.as_deref(),
                None,
                page(None)
            ),
            None
        );
    }

    /// 4.6.3 allows a DateTime to carry a seconds fraction or leave it out,
    /// and both spellings of one instant are the same instant. The window's
    /// own bounds are compared as raw strings nowhere: `.` sorts before `Z`,
    /// so `…:00.500Z` reads as EARLIER than `…:00Z` on a byte compare, and a
    /// Content-Range built from those bounds would name a range the body
    /// contradicts. The instances are sorted on the canonical key already
    /// (`dt_key`); the bounds are on the same key or they disagree with the
    /// order they summarize.
    #[tokio::test]
    async fn the_window_bounds_are_the_true_extremes_across_fraction_spellings() {
        let doc = json!({
            "id": "urn:ngsi-ld:Vehicle:1",
            "type": "Vehicle",
            "speed": [
                {"type": "Property", "value": 1, "observedAt": "2020-01-01T00:09:00Z"},
                {"type": "Property", "value": 2, "observedAt": "2020-01-01T00:09:00.500Z"},
            ],
        });
        let w = windowed(&doc, None, None).await;
        assert_eq!(w.attrs["speed"].len(), 2);
        assert_eq!(
            observed(&w),
            vec!["2020-01-01T00:09:00Z", "2020-01-01T00:09:00.500Z"],
            "the instances themselves sort on the canonical key"
        );
        assert_eq!(w.ts_min.as_deref(), Some("2020-01-01T00:09:00Z"));
        assert_eq!(w.ts_max.as_deref(), Some("2020-01-01T00:09:00.500Z"));
    }
}

#[cfg(test)]
mod forwarded_path_encoding {
    use crate::AppState;
    use axum::body::Body;
    use axum::http::{Request, StatusCode};
    use std::io::{Read, Write};
    use std::sync::{Arc, Mutex};
    use tower::ServiceExt;

    /// An entity id is a URI (4.6.2), and `#` is legal in one. It also ends a
    /// path in RFC 3986 clause 3.3, so the id has to be percent-encoded
    /// wherever it becomes a path segment.
    const ENTITY: &str = "urn:ngsi-ld:Vehicle:temporal-enc#frag";
    const ENCODED: &str = "urn:ngsi-ld:Vehicle:temporal-enc%23frag";

    /// A Context Source answering 204 to everything, recording request lines.
    fn mock_source() -> (u16, Arc<Mutex<Vec<String>>>) {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
        let port = listener.local_addr().expect("addr").port();
        let seen: Arc<Mutex<Vec<String>>> = Arc::default();
        let log = seen.clone();
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(mut s) = stream else { continue };
                let mut buf = [0u8; 8192];
                let n = s.read(&mut buf).unwrap_or(0);
                if let Some(line) = String::from_utf8_lossy(&buf[..n]).lines().next() {
                    log.lock().expect("lock").push(line.to_owned());
                }
                let _ = s.write_all(
                    b"HTTP/1.1 204 No Content\r\nConnection: close\r\nContent-Length: 0\r\n\r\n",
                );
            }
        });
        (port, seen)
    }

    fn state() -> AppState {
        // the mock source is loopback, denied by the egress policy by default
        crate::allow_private();
        AppState::new("antares-temporal-enc".into())
    }

    async fn send(st: &AppState, req: Request<Body>) -> axum::http::Response<Body> {
        crate::router(st.clone())
            .oneshot(req)
            .await
            .expect("response")
    }

    async fn post(st: &AppState, uri: &str, body: String) -> axum::http::Response<Body> {
        let req = Request::builder()
            .method("POST")
            .uri(uri)
            .header("Content-Type", "application/json")
            .header("Content-Length", body.len())
            .body(Body::from(body))
            .expect("request");
        send(st, req).await
    }

    async fn register(st: &AppState, port: u16, id: &str, entity: &str) {
        let doc = serde_json::json!({
            "id": format!("urn:ngsi-ld:ContextSourceRegistration:{id}"),
            "type": "ContextSourceRegistration",
            "mode": "redirect",
            "operations": ["deleteAttrsTemporal", "deleteAttrInstanceTemporal"],
            "information": [{"entities": [{"type": "Vehicle", "id": entity}]}],
            "endpoint": format!("http://127.0.0.1:{port}"),
        });
        assert_eq!(
            post(st, "/ngsi-ld/v1/csourceRegistrations", doc.to_string())
                .await
                .status(),
            StatusCode::CREATED,
            "registration create"
        );
    }

    /// 5.6.13.4/5.6.15.4: the operation is forwarded to the registration
    /// endpoint with the target resource named in the request path. The id,
    /// the Attribute name and the instanceId arrive percent-decoded from this
    /// broker's own path, so splicing them raw would let a `#` end the
    /// forwarded path (RFC 3986 clause 3.3) and turn Delete Attribute into
    /// Delete Temporal Evolution of an Entity (5.6.16) on the peer.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_hash_in_the_id_reaches_the_peer_encoded_not_truncated() {
        let st = state();
        let (port, seen) = mock_source();
        register(&st, port, "csr-temporal-enc", ENTITY).await;

        // each suffix is already in its encoded spelling, so the forwarded
        // path must repeat it verbatim
        for suffix in ["/attrs/speed", "/attrs/speed/urn:ngsi-ld:Instance:1%23x"] {
            let req = Request::builder()
                .method("DELETE")
                .uri(format!("/ngsi-ld/v1/temporal/entities/{ENCODED}{suffix}"))
                .body(Body::empty())
                .expect("request");
            let status = send(&st, req).await.status();
            assert_ne!(status, StatusCode::BAD_REQUEST, "{suffix}");
            let lines = seen.lock().expect("lock").clone();
            let last = lines.last().cloned().unwrap_or_default();
            assert!(
                last.contains(&format!("/ngsi-ld/v1/temporal/entities/{ENCODED}{suffix}")),
                "forwarded request line {last:?} for suffix {suffix}"
            );
            // the negative assertion: the peer must never see a path that
            // stops at the entity resource
            assert!(
                !last.contains("temporal-enc HTTP/"),
                "forwarded path truncated at the `#`: {last:?}"
            );
        }
    }

    /// 5.6.13.4: "If the target Attribute name is not a valid name, then an
    /// error of type BadRequestData shall be raised." A name begins with a
    /// letter (4.6.2), so no valid name is a relative-path dot segment (RFC
    /// 3986 clause 5.2.4) — and such a name in a forwarded path would address
    /// the peer's Temporal Evolution resource instead of its Attribute, so it
    /// is refused before anything leaves this broker.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_dot_segment_attribute_name_is_refused_and_never_forwarded() {
        const TARGET: &str = "urn:ngsi-ld:Vehicle:temporal-dots";
        let st = state();
        let (port, seen) = mock_source();
        register(&st, port, "csr-temporal-dots", TARGET).await;
        // raw, decoded once by this broker, and decoded once more by the peer
        for attr in ["..", "%2e%2e", "%252e%252e", "."] {
            for suffix in ["", "/urn:ngsi-ld:Instance:1"] {
                let req = Request::builder()
                    .method("DELETE")
                    .uri(format!(
                        "/ngsi-ld/v1/temporal/entities/{TARGET}/attrs/{attr}{suffix}"
                    ))
                    .body(Body::empty())
                    .expect("request");
                assert_eq!(
                    send(&st, req).await.status(),
                    StatusCode::BAD_REQUEST,
                    "attribute name {attr:?} with suffix {suffix:?}"
                );
            }
        }
        assert!(
            seen.lock().expect("lock").is_empty(),
            "a rejected attribute name must never reach a registration endpoint"
        );
    }

    /// 5.6.11.4: on creation the response carries a Location header holding
    /// the resource URI of the created Temporal Representation. A URI has its
    /// reserved characters percent-encoded (RFC 3986 clause 3.3), so a `#` in
    /// the id may not be spliced raw — there it would read as the start of a
    /// fragment identifier and address the entity collection.
    #[tokio::test(flavor = "multi_thread")]
    async fn the_location_header_percent_encodes_the_id() {
        let st = state();
        let doc = serde_json::json!({
            "id": ENTITY, "type": "Vehicle",
            "speed": [{"type": "Property", "value": 1,
                       "observedAt": "2026-03-01T12:05:00Z"}],
        });
        let res = post(&st, "/ngsi-ld/v1/temporal/entities", doc.to_string()).await;
        assert_eq!(res.status(), StatusCode::CREATED);
        assert_eq!(
            res.headers()
                .get("Location")
                .and_then(|v| v.to_str().ok())
                .unwrap_or_default(),
            format!("/ngsi-ld/v1/temporal/entities/{ENCODED}")
        );
    }
}

#[cfg(test)]
mod clause_4_5_19 {
    use super::*;
    use serde_json::json;

    /// 4.5.19.1: "The duration shall be a string in the format
    /// `P[n]Y[n]M[n]DT[n]H[n]M[n]S` or `P[n]W` … For example,
    /// `"P3Y6M4DT12H30M5S"` represents a duration of "three years, six
    /// months, four days, twelve hours, thirty minutes, and five seconds"."
    /// A period mixing date and time elements is therefore valid, and
    /// "PT0S" spans the whole time range of the query.
    #[test]
    fn a_mixed_date_and_time_duration_is_a_valid_period() {
        const DAY: i64 = 86_400;
        assert_eq!(
            parse_aggr_period("P3Y6M4DT12H30M5S"),
            Some(AggrPeriod::Months(42, 4 * DAY + 12 * 3600 + 30 * 60 + 5))
        );
        assert_eq!(
            parse_aggr_period("P1Y1D"),
            Some(AggrPeriod::Months(12, DAY))
        );
        assert_eq!(
            parse_aggr_period("P1MT1H"),
            Some(AggrPeriod::Months(1, 3600))
        );
        // the pure forms are unchanged
        assert_eq!(parse_aggr_period("PT0S"), Some(AggrPeriod::Whole));
        assert_eq!(parse_aggr_period("P0D"), Some(AggrPeriod::Whole));
        assert_eq!(parse_aggr_period("P1M"), Some(AggrPeriod::Months(1, 0)));
        assert_eq!(parse_aggr_period("PT90M"), Some(AggrPeriod::Seconds(5400)));
        assert_eq!(parse_aggr_period("P1W"), Some(AggrPeriod::Seconds(7 * DAY)));
        // and the grammar still rejects what is not a duration
        assert_eq!(parse_aggr_period("P1X"), None);
        assert_eq!(parse_aggr_period("1Y"), None);
        assert_eq!(parse_aggr_period("P1"), None);
    }

    fn windowed(times: &[&str]) -> Windowed {
        let instances: Vec<Value> = times
            .iter()
            .map(|t| json!({"type": "Property", "value": 1, "observedAt": t}))
            .collect();
        let mut attrs = std::collections::BTreeMap::new();
        attrs.insert("speed".to_owned(), instances);
        Windowed {
            attrs,
            max_per_attr: times.len(),
            ts_min: times.first().map(|s| (*s).to_owned()),
            ts_max: times.last().map(|s| (*s).to_owned()),
            truncated: false,
            cut_served: None,
            more: false,
            held: Default::default(),
        }
    }

    fn repr(duration: &str) -> TRepr {
        TRepr {
            aggregated: true,
            aggr_methods: vec!["totalCount".to_owned()],
            aggr_period: parse_aggr_period(duration).expect("duration"),
            ..Default::default()
        }
    }

    /// The periods of an aggregated response are the periods "in the time
    /// range of the query" (4.5.19.0), so with `timerel=before` they run
    /// backwards from `timeAt` and every returned period contains the
    /// instances aggregated into it.
    #[tokio::test]
    async fn month_periods_before_the_anchor_contain_their_instances() {
        let w = windowed(&[
            "2020-01-15T00:00:00Z",
            "2020-02-15T00:00:00Z",
            "2020-03-15T00:00:00Z",
        ]);
        let tq = TemporalQ {
            timerel: "before".to_owned(),
            time_at: "2020-04-01T00:00:00Z".to_owned(),
            end_time_at: None,
            timeproperty: "observedAt".to_owned(),
        };
        let out = render_aggregated(
            &w,
            Some(&tq),
            &repr("P1M"),
            &antares_jsonld::Context::default(),
            "observedAt",
        )
        .expect("aggregated");
        let rows = out["speed"]["totalCount"].as_array().expect("rows").clone();
        assert_eq!(
            rows,
            vec![
                json!([1, "2020-01-01T00:00:00Z", "2020-02-01T00:00:00Z"]),
                json!([1, "2020-02-01T00:00:00Z", "2020-03-01T00:00:00Z"]),
                json!([1, "2020-03-01T00:00:00Z", "2020-04-01T00:00:00Z"]),
            ]
        );
        // the negative assertion: no period may start at the anchor, since
        // such a period holds none of the instances of a `before` query
        assert!(
            !Value::Array(rows)
                .to_string()
                .contains("2020-04-01T00:00:00Z\",\""),
            "a period starting at timeAt contains no instance of a before query"
        );
    }

    /// 4.5.19.1: "A duration of 0 second (e.g. expressed as "PT0S" or
    /// "P0D") is valid and is interpreted as a duration spanning the whole
    /// time range specified by the temporal query." The query names one
    /// edge of that range and 4.11 leaves the other open, so the period
    /// runs from `timeAt` only when `timeAt` is where the range starts.
    #[tokio::test]
    async fn the_zero_duration_period_spans_the_time_range_the_query_asked_for() {
        let w = windowed(&["2020-09-01T12:03:00Z", "2020-09-01T12:05:00Z"]);
        let one = |tq: &TemporalQ| {
            let out = render_aggregated(
                &w,
                Some(tq),
                &repr("PT0S"),
                &antares_jsonld::Context::default(),
                "observedAt",
            )
            .expect("aggregated");
            out["speed"]["totalCount"].as_array().expect("rows").clone()
        };

        // before: timeAt ENDS the range (4.11 makes the start open, so the
        // data supplies it). The period must not run backwards.
        assert_eq!(
            one(&TemporalQ {
                timerel: "before".to_owned(),
                time_at: "2030-01-01T00:00:00Z".to_owned(),
                end_time_at: None,
                timeproperty: "observedAt".to_owned(),
            }),
            vec![json!([2, "2020-09-01T12:03:00Z", "2030-01-01T00:00:00Z"])]
        );

        // between: both edges are named, and the period is exactly them —
        // not the last instant the data happens to hold.
        assert_eq!(
            one(&TemporalQ {
                timerel: "between".to_owned(),
                time_at: "2020-09-01T12:00:00Z".to_owned(),
                end_time_at: Some("2020-09-01T13:00:00Z".to_owned()),
                timeproperty: "observedAt".to_owned(),
            }),
            vec![json!([2, "2020-09-01T12:00:00Z", "2020-09-01T13:00:00Z"])]
        );

        // after: timeAt STARTS the range, the data closes it.
        assert_eq!(
            one(&TemporalQ {
                timerel: "after".to_owned(),
                time_at: "2020-01-01T00:00:00Z".to_owned(),
                end_time_at: None,
                timeproperty: "observedAt".to_owned(),
            }),
            vec![json!([2, "2020-01-01T00:00:00Z", "2020-09-01T12:05:01Z"])]
        );
    }

    /// A mixed period steps by its months AND its seconds: "P1MT12H" from
    /// the anchor ends one month and twelve hours later (4.5.19.1).
    #[tokio::test]
    async fn a_mixed_period_steps_by_both_components() {
        let w = windowed(&["2020-01-01T06:00:00Z", "2020-02-02T06:00:00Z"]);
        let tq = TemporalQ {
            timerel: "after".to_owned(),
            time_at: "2020-01-01T00:00:00Z".to_owned(),
            end_time_at: None,
            timeproperty: "observedAt".to_owned(),
        };
        let out = render_aggregated(
            &w,
            Some(&tq),
            &repr("P1MT12H"),
            &antares_jsonld::Context::default(),
            "observedAt",
        )
        .expect("aggregated");
        assert_eq!(
            out["speed"]["totalCount"],
            json!([
                [1, "2020-01-01T00:00:00Z", "2020-02-01T12:00:00Z"],
                [1, "2020-02-01T12:00:00Z", "2020-03-02T00:00:00Z"],
            ])
        );
    }

    /// One attribute whose instances carry the given members, an hour apart.
    fn windowed_props(members: &[Value]) -> Windowed {
        let instances: Vec<Value> = members
            .iter()
            .enumerate()
            .map(|(i, m)| {
                let mut inst = json!({
                    "type": "Property",
                    "observedAt": format!("2020-01-01T{i:02}:00:00Z"),
                });
                for (k, v) in m.as_object().expect("instance members") {
                    inst[k] = v.clone();
                }
                inst
            })
            .collect();
        let times: Vec<String> = instances
            .iter()
            .map(|i| i["observedAt"].as_str().unwrap_or_default().to_owned())
            .collect();
        let mut attrs = std::collections::BTreeMap::new();
        attrs.insert("speed".to_owned(), instances);
        Windowed {
            attrs,
            max_per_attr: members.len(),
            ts_min: times.first().cloned(),
            ts_max: times.last().cloned(),
            truncated: false,
            cut_served: None,
            more: false,
            held: Default::default(),
        }
    }

    fn aggregate(w: &Windowed, methods: &[&str]) -> Result<Map<String, Value>, NgsiError> {
        let r = TRepr {
            aggregated: true,
            aggr_methods: methods.iter().map(|m| (*m).to_string()).collect(),
            ..Default::default()
        };
        render_aggregated(
            w,
            None,
            &r,
            &antares_jsonld::Context::default(),
            "observedAt",
        )
    }

    /// Table 4.5.19.1-2: on a DateTime and on a Date, `min` "calculates the
    /// minimum value inside the period" and `max` the maximum. Both
    /// datatypes reach the broker as a JSON-LD typed value — C.6's
    /// `{"@type": "DateTime", "@value": "2018-12-04T12:00:00Z"}` — so the
    /// aggregation reads the value through its type instead of treating the
    /// wrapper as an opaque object.
    #[test]
    fn a_date_time_and_a_date_have_a_minimum_and_a_maximum() {
        let dt = |v: &str| json!({"value": {"@type": "DateTime", "@value": v}});
        let out = aggregate(
            &windowed_props(&[
                dt("2020-03-01T00:00:00Z"),
                dt("2020-01-01T00:00:00Z"),
                dt("2020-02-01T00:00:00Z"),
            ]),
            &["min", "max"],
        )
        .expect("DateTime is eligible for min and max");
        assert_eq!(
            out["speed"]["min"][0][0],
            json!({"@type": "DateTime", "@value": "2020-01-01T00:00:00Z"})
        );
        assert_eq!(
            out["speed"]["max"][0][0],
            json!({"@type": "DateTime", "@value": "2020-03-01T00:00:00Z"})
        );

        let d = |v: &str| json!({"value": {"@type": "Date", "@value": v}});
        let out = aggregate(
            &windowed_props(&[d("2020-03-01"), d("2020-01-01")]),
            &["min", "max"],
        )
        .expect("Date is eligible for min and max");
        assert_eq!(
            out["speed"]["min"][0][0],
            json!({"@type": "Date", "@value": "2020-01-01"})
        );
        assert_eq!(
            out["speed"]["max"][0][0],
            json!({"@type": "Date", "@value": "2020-03-01"})
        );
    }

    /// C.6 gives a second representation of the same datatypes: the value
    /// stays a string and `valueType` (4.5.2.2) carries the type, coerced to
    /// its datatype URI on the way in. Table 4.5.19.1-2 applies to the
    /// datatype, not to the spelling, so this form aggregates identically.
    #[test]
    fn a_value_type_carries_the_datatype_as_far_as_the_typed_value_does() {
        let dt =
            |v: &str| json!({"value": v, "valueType": "https://uri.etsi.org/ngsi-ld/DateTime"});
        let out = aggregate(
            &windowed_props(&[dt("2020-03-01T00:00:00Z"), dt("2020-01-01T00:00:00Z")]),
            &["min", "max"],
        )
        .expect("a valueType-coerced DateTime is eligible for min and max");
        assert_eq!(out["speed"]["min"][0][0], json!("2020-01-01T00:00:00Z"));
        assert_eq!(out["speed"]["max"][0][0], json!("2020-03-01T00:00:00Z"));
    }

    /// Table 4.5.19.1-2, Time column: `avg` "calculates the average time
    /// inside the period", and min/max apply as well. 4.6.3 mandates
    /// `hh:mm:ssZ` for a Time, so the computed average is one — carrying its
    /// type, since a bare string would read back as a JSON String, whose own
    /// column in Table 4.5.19.1-1 has no average at all.
    #[test]
    fn a_time_has_an_average_a_minimum_and_a_maximum() {
        let t = |v: &str| json!({"value": {"@type": "Time", "@value": v}});
        let out = aggregate(
            &windowed_props(&[t("09:30:00Z"), t("08:30:00Z")]),
            &["avg", "min", "max"],
        )
        .expect("Time is eligible for avg, min and max");
        assert_eq!(
            out["speed"]["avg"][0][0],
            json!({"@type": "Time", "@value": "09:00:00Z"})
        );
        assert_eq!(
            out["speed"]["min"][0][0],
            json!({"@type": "Time", "@value": "08:30:00Z"})
        );
        assert_eq!(
            out["speed"]["max"][0][0],
            json!({"@type": "Time", "@value": "09:30:00Z"})
        );
    }

    /// The N/A cells of Table 4.5.19.1-2 are refused, not computed: 5.7.4.4
    /// p.211 raises InvalidRequest when an Attribute "is not eligible for at
    /// least one of the aggregation methods specified in the request".
    /// DateTime and Date have no avg, sum, stddev or sumsq; Time has no sum,
    /// stddev or sumsq.
    #[test]
    fn the_methods_a_temporal_datatype_does_not_support_are_refused() {
        let w = windowed_props(&[
            json!({"value": {"@type": "DateTime", "@value": "2020-01-01T00:00:00Z"}}),
        ]);
        for method in ["avg", "sum", "stddev", "sumsq"] {
            assert!(
                matches!(aggregate(&w, &[method]), Err(NgsiError::InvalidRequest(_))),
                "DateTime must not be eligible for {method}"
            );
        }
        let w = windowed_props(&[json!({"value": {"@type": "Time", "@value": "08:30:00Z"}})]);
        for method in ["sum", "stddev", "sumsq"] {
            assert!(
                matches!(aggregate(&w, &[method]), Err(NgsiError::InvalidRequest(_))),
                "Time must not be eligible for {method}"
            );
        }
    }

    /// Table 4.5.19.1-1, JSON String column: `avg` is N/A. A string is a
    /// JSON String whatever it spells, so a value that reads like a
    /// time-of-day is averaged only when its datatype says it is a Time.
    #[test]
    fn a_json_string_has_no_average_however_it_reads() {
        let w = windowed_props(&[json!({"value": "08:30:00Z"}), json!({"value": "09:30:00Z"})]);
        assert!(matches!(
            aggregate(&w, &["avg"]),
            Err(NgsiError::InvalidRequest(_))
        ));
        // its own row of the table is unchanged: lexicographic min and max
        let out = aggregate(&w, &["min", "max"]).expect("a string has min and max");
        assert_eq!(out["speed"]["min"][0][0], json!("08:30:00Z"));
        assert_eq!(out["speed"]["max"][0][0], json!("09:30:00Z"));
    }

    /// The counting methods have no N/A cell in any of the three tables:
    /// `totalCount` "the number of times the value has been updated" and
    /// `distinctCount` "the count of distinct values", for every datatype
    /// including the ones with no other method at all.
    #[test]
    fn every_datatype_is_counted() {
        for members in [
            json!({"value": {"@type": "DateTime", "@value": "2020-01-01T00:00:00Z"}}),
            json!({"value": {"@type": "Date", "@value": "2020-01-01"}}),
            json!({"value": {"@type": "Time", "@value": "08:30:00Z"}}),
            json!({"vocab": "urn:ngsi-ld:Colour:red"}),
            json!({"object": "urn:ngsi-ld:Car:1"}),
        ] {
            let w = windowed_props(&[members.clone(), members.clone()]);
            let out = aggregate(&w, &["totalCount", "distinctCount"])
                .unwrap_or_else(|e| panic!("{members} must be counted: {e:?}"));
            assert_eq!(out["speed"]["totalCount"][0][0], json!(2));
            assert_eq!(out["speed"]["distinctCount"][0][0], json!(1));
        }
    }
}

#[cfg(test)]
mod clause_4_21 {
    use super::*;
    use antares_jsonld::Loader;

    fn params(kv: &[(&str, &str)]) -> HashMap<String, String> {
        kv.iter()
            .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
            .collect()
    }

    /// 4.21 Projections: "pick, omit and attrs are mutually exclusive" holds
    /// on the temporal representation exactly as it does on the current-state
    /// one — the temporal operations define no exception to it, so any pair
    /// is BadRequestData and each one alone parses.
    #[test]
    fn pick_omit_and_attrs_cannot_be_combined_on_a_temporal_query() {
        let ctx = Loader::new().core();
        for p in [
            params(&[("pick", "a"), ("omit", "b")]),
            params(&[("pick", "a"), ("attrs", "b")]),
            params(&[("omit", "a"), ("attrs", "b")]),
            params(&[("pick", "a"), ("omit", "b"), ("attrs", "c")]),
        ] {
            match parse_trepr(&p, &ctx) {
                Err(NgsiError::BadRequestData(_)) => {}
                other => panic!("must be BadRequestData, got {:?}", other.err()),
            }
        }
        for p in [
            params(&[("pick", "a")]),
            params(&[("omit", "a")]),
            params(&[("attrs", "a")]),
        ] {
            assert!(parse_trepr(&p, &ctx).is_ok());
        }
    }

    /// 4.21 on the core members of a temporal Entity: `pick` constrains them
    /// strictly (only what is named survives) and `omit` drops a named member
    /// only when the node carries no children — the same reading the
    /// current-state representation applies, so the two never disagree about
    /// whether `id` or `type` is in the answer.
    #[test]
    fn core_members_follow_the_same_projection_rule() {
        let pick = crate::repr::parse_projection("id", &Loader::new().core()).expect("pick");
        assert!(crate::repr::meta_projected(Some(&pick), None, "id"));
        assert!(!crate::repr::meta_projected(Some(&pick), None, "type"));
        let omit = crate::repr::parse_projection("type", &Loader::new().core()).expect("omit");
        assert!(!crate::repr::meta_projected(None, Some(&omit), "type"));
        assert!(crate::repr::meta_projected(None, Some(&omit), "id"));
        assert!(crate::repr::meta_projected(None, None, "type"));
    }
}

#[cfg(test)]
mod t3932_time_of_day {
    use super::seconds_of_day;

    /// T-3932: a Time read with checked byte positions; a short or
    /// multi-byte value is not a Time, never a panic.
    #[test]
    fn a_short_or_multibyte_time_is_none() {
        assert_eq!(seconds_of_day("12:30:00Z"), Some(45_000.0));
        assert_eq!(seconds_of_day("12:30:00.5"), Some(45_000.5));
        for bad in ["", "12:3", "12:30", "1é:00:00", "12-30-00", "aa:bb:cc"] {
            assert_eq!(seconds_of_day(bad), None, "{bad}");
        }
    }
}
