// SPDX-License-Identifier: EUPL-1.2
//! Batch operations /entityOperations/* (5.6.7–5.6.10, 5.6.20, 5.7.2-POST;
//! resources 6.14–6.17, 6.23, 6.31).

use crate::entities::merge_into;
use crate::negotiate::*;
use crate::paging::{paginate, query_doc_params};
use crate::repr::{apply, parse_repr};
use crate::stamp::stamp_new;
use crate::state::{now_iso, AppState};
use antares_jsonld::{expand_entity, ExpandOpts};
use antares_model::NgsiError;
use antares_store::CurrentStateDriverExt;
use axum::body::Bytes;
use axum::extract::State;
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use serde_json::{json, Value};
use std::collections::HashMap;

use crate::negotiate::CleanParams;

/// Parse a batch body: JSON array of entity documents; per-document context
/// resolution (ld+json ⇒ each doc's own @context; json ⇒ Link header).
/// Batch bodies are the ingest hot path — sonic-rs (3–4× parse) behind
/// the `sonic` feature, serde_json always compiled as the fallback.
#[cfg(feature = "sonic")]
fn parse_batch_body(body: &[u8]) -> Result<Value, String> {
    sonic_rs::from_slice(body).map_err(|e| e.to_string())
}
#[cfg(not(feature = "sonic"))]
fn parse_batch_body(body: &[u8]) -> Result<Value, String> {
    serde_json::from_slice(body).map_err(|e| e.to_string())
}

/// One entry of a batch array as it arrives: the item and the result of
/// resolving the `@context` that governs it. A failed resolution is carried
/// per item, because 5.6.7 answers the array with one error per entity.
type BatchItem = (Value, ApiResult<std::sync::Arc<antares_jsonld::Context>>);

/// One entry of a batch as a forward sends it: the object and the `@context`
/// that expanded it.
type FwdItem = (
    serde_json::Map<String, Value>,
    std::sync::Arc<antares_jsonld::Context>,
);

/// One batch body into (item, its resolved @context) pairs. Each item's
/// `@context` — or the Link header standing in for it — resolves within the
/// requesting Tenant (5.5.10): a Hosted @context belongs to the Tenant that
/// stored it (5.13.1), so for any other Tenant the URL is unresolvable rather
/// than a set of term mappings the Entity would be expanded through.
async fn parse_batch(
    st: &AppState,
    tenant: &antares_model::TenantId,
    headers: &HeaderMap,
    body: &[u8],
) -> ApiResult<Vec<BatchItem>> {
    let ct = content_type(headers)?;
    let ld = match ct.as_str() {
        "application/json" => false,
        "application/ld+json" => true,
        _ => return Err(ApiError::Bare(StatusCode::UNSUPPORTED_MEDIA_TYPE)),
    };
    let value: Value = parse_batch_body(body)
        .map_err(|e| NgsiError::InvalidRequest(format!("body is not valid JSON: {e}")))?;
    let items = value.as_array().filter(|a| !a.is_empty()).ok_or_else(|| {
        NgsiError::BadRequestData("batch body must be a non-empty JSON array".into())
    })?;
    // 5.6.7.4 (and siblings): a null value in ANY item fails the whole
    // request with BadRequestData — not a per-item 207 error.
    if items.iter().any(Value::is_null) {
        return Err(
            NgsiError::BadRequestData("batch array must not contain null items".into()).into(),
        );
    }
    // Batch entity count cap
    if items.len() > *crate::bounds::MAX_BATCH_ITEMS {
        return Err(NgsiError::BadRequestData(format!(
            "batch of {} exceeds the {}-entity limit",
            items.len(),
            *crate::bounds::MAX_BATCH_ITEMS
        ))
        .into());
    }
    let link = link_context(headers)?;
    if ld && link.is_some() {
        return Err(NgsiError::BadRequestData(
            "application/ld+json batch must not also carry a Link @context (6.3.5)".into(),
        )
        .into());
    }
    let mut out = Vec::new();
    // The loader caps ONE @context resolution at MAX_CONTEXT_FETCHES fetched
    // documents. A batch resolves once per item, so without a ceiling here
    // the item count multiplies that cap: a body inside the 4 MiB limit can
    // name a thousand different @contexts and buy a thousand crawls of a
    // chosen host from one request. Distinct values are what cost a crawl —
    // repeats hit the loader's merged cache, and an inline object fetches
    // nothing — so only those are counted.
    let mut named_contexts: std::collections::HashSet<String> = std::collections::HashSet::new();
    let mut count_context = |c: &Value| -> Result<(), ApiError> {
        if matches!(c, Value::Object(_) | Value::Null) {
            return Ok(());
        }
        let key = c.to_string();
        if named_contexts.contains(&key) {
            return Ok(());
        }
        // At the ceiling nothing more is remembered either: the set is keyed
        // by client-supplied text, so it may not grow past the cap it guards.
        if named_contexts.len() >= crate::bounds::MAX_CONTEXT_FETCHES {
            return Err(NgsiError::BadRequestData(format!(
                "batch names more than {} distinct @contexts",
                crate::bounds::MAX_CONTEXT_FETCHES
            ))
            .into());
        }
        named_contexts.insert(key);
        Ok(())
    };
    for item in items {
        let ctx = if ld {
            match item.get("@context") {
                Some(c) => match count_context(c) {
                    Err(e) => Err(e),
                    Ok(()) => st
                        .loader
                        .resolve_for(tenant, c)
                        .await
                        .map_err(ApiError::from),
                },
                None => Err(NgsiError::BadRequestData(
                    "ld+json batch entity without @context".into(),
                )
                .into()),
            }
        } else if item.get("@context").is_some() {
            Err(NgsiError::BadRequestData("application/json entity carries @context".into()).into())
        } else {
            match &link {
                Some(url) => st
                    .loader
                    .resolve_for(tenant, &Value::String(url.clone()))
                    .await
                    .map_err(ApiError::from),
                None => Ok(st.loader.core()),
            }
        };
        out.push((item.clone(), ctx));
    }
    Ok(out)
}

struct BatchOutcome {
    success: Vec<Value>,
    errors: Vec<Value>,
}

impl BatchOutcome {
    /// Batch output data (5.6.7.5 / 5.6.8.5 / 5.6.9.5 / 5.6.10.5): if every
    /// entity succeeded, the operation's all-ok status (201 + id array for
    /// create, else 204 with no body); otherwise 207 with the S array
    /// ("success") and the E array of BatchEntityError 5.2.17 ("errors").
    fn respond(
        self,
        tenant: &antares_model::TenantId,
        all_ok_status: StatusCode,
        body_on_ok: bool,
    ) -> Response {
        if self.errors.is_empty() {
            if body_on_ok {
                let mut resp = (
                    all_ok_status,
                    [(axum::http::header::CONTENT_TYPE, "application/json")],
                    axum::Json(Value::Array(self.success)),
                )
                    .into_response();
                echo_tenant(tenant, &mut resp);
                resp
            } else {
                let mut resp = all_ok_status.into_response();
                echo_tenant(tenant, &mut resp);
                resp
            }
        } else {
            multi_status(
                json!({"success": self.success, "errors": self.errors}),
                tenant,
            )
        }
    }
}

/// 5.2.17 BatchEntityError for a member the policy engine refused: the
/// AccessDenied problem a refused single-Entity write answers with (ADR-0020).
fn denied_entry(id: Option<&str>, why: &str) -> Value {
    json!({
        "entityId": id.unwrap_or("unknown"),
        "error": {
            "type": crate::policy::ACCESS_DENIED_TYPE,
            "title": crate::policy::ACCESS_DENIED_TITLE,
            "status": StatusCode::FORBIDDEN.as_u16(),
            "detail": why,
        },
    })
}

/// The reason a batch is refused whole: every member was refused, so the
/// answer is the 403 a single refusal gives, carrying the first reason.
fn whole_refusal(refused: &[Result<crate::policy::Filter, String>]) -> Option<String> {
    if refused.iter().all(Result::is_err) {
        refused.first().and_then(|r| r.clone().err())
    } else {
        None
    }
}

/// The conditions the engine set on the allowed members (gap G12), by id.
/// One id named by several members holds to every condition any of them
/// carries: a round writes each id once, so the strictest reading is the
/// one that never applies a member's write past its own condition.
#[derive(Default)]
struct Conditions(HashMap<String, Vec<crate::policy::Condition>>);

impl Conditions {
    fn add(&mut self, id: Option<&str>, granted: &crate::policy::Filter) {
        if let (Some(id), Some(c)) = (id, crate::policy::Condition::of(granted)) {
            self.0.entry(id.to_owned()).or_default().push(c);
        }
    }

    fn holds(&self, id: &str, doc: &Value) -> bool {
        self.0
            .get(id)
            .is_none_or(|cs| cs.iter().all(|c| c.holds(doc)))
    }

    /// Inside the store closure: a member whose stored Entity a condition
    /// does not select is not found, as an absent one is.
    fn pre(&self, id: &str, doc: &Value) -> Result<(), NgsiError> {
        if self.holds(id, doc) {
            Ok(())
        } else {
            Err(Self::not_found(id))
        }
    }

    /// Whether a condition holds `id`: an upsert of it may update an Entity
    /// the condition selects and never creates one (5.6.8, T-3668).
    fn has(&self, id: &str) -> bool {
        self.0.contains_key(id)
    }

    /// The one answer for a conditioned member that is absent or not
    /// selected, so the two cannot be told apart.
    fn not_found(id: &str) -> NgsiError {
        NgsiError::ResourceNotFound(format!("entity {id} not found"))
    }

    fn unforwarded(&self, forwards: bool) -> Result<(), crate::policy::Denied> {
        if forwards && !self.0.is_empty() {
            Err(crate::policy::Denied(
                crate::policy::CONDITION_NOT_FORWARDED.to_owned(),
            ))
        } else {
            Ok(())
        }
    }
}

/// The members of a batch round a view lets through its Relationship check,
/// each with the guard its store closure applies (ADR-0023 rule 7); the
/// others land in E. Without a view every member goes, with no guard. The
/// round's targets are read together (T-4085), apart from those `known`
/// already holds: Entities the batch read before its first write and does not
/// itself write, so no round has changed them.
#[cfg(feature = "views")]
async fn view_guards(
    st: &AppState,
    tenant: &antares_model::TenantId,
    round: Vec<(String, Value)>,
    known: &HashMap<String, Value>,
    out: &mut BatchOutcome,
) -> (
    Vec<(String, Value)>,
    HashMap<String, crate::views::WriteGuard>,
) {
    let members: Vec<(String, Value, Option<crate::views::WriteGuard>)> = round
        .into_iter()
        .map(|(id, doc)| {
            let names = crate::attrs::attr_iris_of(&doc);
            let g = crate::views::WriteGuard::of(st, names.iter().map(String::as_str));
            (id, doc, g)
        })
        .collect();
    let mut missing: Vec<&str> = members
        .iter()
        .filter(|(_, _, g)| g.is_some())
        .flat_map(|(_, doc, _)| crate::views::relationship_targets(doc))
        .filter(|t| !known.contains_key(*t))
        .collect();
    missing.sort_unstable();
    missing.dedup();
    let mut found = if missing.is_empty() {
        HashMap::new()
    } else {
        // A store failure reads as absent, as a failed single read did.
        st.store
            .get_entities(tenant, &missing)
            .await
            .unwrap_or_default()
    };
    for t in members
        .iter()
        .filter(|(_, _, g)| g.is_some())
        .flat_map(|(_, doc, _)| crate::views::relationship_targets(doc))
    {
        if let Some(d) = known.get(t) {
            found.entry(t.to_owned()).or_insert_with(|| d.clone());
        }
    }
    let mut guards = HashMap::new();
    let mut kept = Vec::with_capacity(members.len());
    for (id, doc, g) in members {
        let Some(g) = g else {
            kept.push((id, doc));
            continue;
        };
        match g.targets_in(&doc, &found) {
            Ok(()) => {
                guards.insert(id.clone(), g);
                kept.push((id, doc));
            }
            Err(crate::policy::Denied(why)) => out.errors.push(denied_entry(Some(&id), &why)),
        }
    }
    (kept, guards)
}

/// One round of a Batch Upsert under a view, member by member (ADR-0023
/// rule 7, T-3594): a stored Entity the view shows is replaced, or with
/// `options=update` updated, under the store's lock between the pre-image
/// and post-image checks; a replace counts every Attribute the Entity holds
/// as written, so one holding a hidden Attribute is refused (D6). An absent
/// id, and one out of the view, take the create path: the Entity is created
/// in the view or refused, and an out-of-view one is never overwritten,
/// answering as Create Entity through the view does for a taken id. The ids
/// created, in order.
#[cfg(feature = "views")]
#[allow(clippy::too_many_arguments)]
async fn upsert_in_view(
    st: &AppState,
    tenant: &antares_model::TenantId,
    round: Vec<(String, Value)>,
    update_mode: bool,
    ts: &str,
    conds: &Conditions,
    known: &HashMap<String, Value>,
    out: &mut BatchOutcome,
) -> Result<Vec<String>, NgsiError> {
    let (round, guards) = view_guards(st, tenant, round, known, out).await;
    let ids: Vec<String> = round.iter().map(|(id, _)| id.clone()).collect();
    let docs: HashMap<&str, &Value> = round.iter().map(|(id, d)| (id.as_str(), d)).collect();
    let res = st
        .store
        .batch_mutate(tenant, &ids, |id, doc| {
            conds.pre(id, doc)?;
            let guard = guards.get(id);
            if let Some(g) = guard {
                if update_mode {
                    g.pre(id, doc)?;
                } else {
                    g.pre_whole(id, doc)?;
                }
            }
            let Some(&src) = docs.get(id) else {
                return Err(NgsiError::InternalError(format!(
                    "batch member {id} lost its document"
                )));
            };
            if update_mode {
                merge_into(doc, src, ts);
            } else {
                let mut new = src.clone();
                stamp_new(&mut new, ts);
                // 4.8: a replaced Entity keeps the instant it was created.
                if let (Some(o), Some(created)) = (new.as_object_mut(), doc.get("createdAt")) {
                    o.insert("createdAt".into(), created.clone());
                }
                *doc = new;
            }
            if let Some(g) = guard {
                g.post(doc)?;
            }
            Ok::<(), NgsiError>(())
        })
        .await?;
    let mut creates = Vec::new();
    for ((id, doc), r) in round.into_iter().zip(res) {
        if let Some(crate::policy::Denied(why)) = guards.get(&id).and_then(|g| g.verdict().err()) {
            out.errors.push(denied_entry(Some(&id), &why));
            continue;
        }
        match r {
            Some(Ok(())) => {
                if !out.success.contains(&Value::String(id.clone())) {
                    out.success.push(Value::String(id));
                }
            }
            // a conditioned member never creates: absent or not selected,
            // it is not found
            None | Some(Err(NgsiError::ResourceNotFound(_))) if conds.has(&id) => {
                out.errors
                    .push(err_entry(Some(&id), &Conditions::not_found(&id)));
            }
            // absent, or out of the view: the pre-image answers not found
            None | Some(Err(NgsiError::ResourceNotFound(_))) => creates.push((id, doc)),
            Some(Err(e)) => out.errors.push(err_entry(Some(&id), &e)),
        }
    }
    let mut kept = Vec::with_capacity(creates.len());
    for (id, mut doc) in creates {
        match crate::views::admits_new(st, tenant, &doc).await {
            Ok(()) => {
                stamp_new(&mut doc, ts);
                kept.push((id, doc));
            }
            Err(crate::policy::Denied(why)) => out.errors.push(denied_entry(Some(&id), &why)),
        }
    }
    let mut created = Vec::new();
    if kept.is_empty() {
        return Ok(created);
    }
    let ids: Vec<String> = kept.iter().map(|(id, _)| id.clone()).collect();
    let flags = st.store.batch_create(tenant, kept).await?;
    for (id, ok) in ids.into_iter().zip(flags) {
        if !ok {
            let e = NgsiError::AlreadyExists(format!("entity {id} already exists"));
            out.errors.push(err_entry(Some(&id), &e));
        } else if !out.success.contains(&Value::String(id.clone())) {
            out.success.push(Value::String(id.clone()));
            created.push(id);
        }
    }
    Ok(created)
}

fn err_entry(id: Option<&str>, e: &NgsiError) -> Value {
    json!({
        "entityId": id.unwrap_or("unknown"),
        "error": problem_value(e),
    })
}

/// 5.2.17 BatchEntityError for a failed forwarded part — the remote status
/// and detail travel inside the ProblemDetails (5.6.7.4/5.6.10.4: remote
/// error results merge into E).
/// The status a Context Source put in a batch error entry, when it is an
/// HTTP error status; anything else (absent, not a number, outside 400..=599,
/// past u16) is the status its response came back with. A plain `as u16`
/// wrapped 65 736 to 200 and reported the failure as a success.
fn remote_status(entry: &Value, fallback: u16) -> u16 {
    entry
        .get("error")
        .and_then(|p| p.get("status"))
        .and_then(Value::as_u64)
        .and_then(|s| u16::try_from(s).ok())
        .filter(|s| (400..=599).contains(s))
        .unwrap_or(fallback)
}

fn err_remote(id: Option<&str>, status: u16, detail: &str) -> Value {
    let etype = match status {
        400 => "BadRequestData",
        404 => "ResourceNotFound",
        409 => "AlreadyExists",
        422 => "OperationNotSupported",
        _ => "InternalError",
    };
    json!({
        "entityId": id.unwrap_or("unknown"),
        "error": {
            "type": format!("https://uri.etsi.org/ngsi-ld/errors/{etype}"),
            "title": "distributed operation failed",
            "status": status,
            "detail": detail,
        }
    })
}

/// Merge one forwarded BATCH operation's outcome into the remote S/E sets
/// (5.6.7.4: "Merge the returned list of Entities successfully created with
/// S. Merge the returned list of Entities in Error with E.").
fn merge_remote_batch(
    status: u16,
    body: &Value,
    sent_ids: &[String],
    created: bool,
    ok: &mut Vec<(String, bool)>,
    err: &mut Vec<Value>,
) {
    // Only the Entities this broker forwarded may appear in the client's
    // S/E arrays: an id the Context Source names but we never sent is not
    // part of this request's outcome, and the source's own error text is
    // rebuilt from its status rather than relayed verbatim.
    let mine = |id: &str| sent_ids.iter().any(|s| s == id);
    match (status, body) {
        (200..=206, Value::Array(a)) => {
            for id in a.iter().filter_map(Value::as_str).filter(|i| mine(i)) {
                ok.push((id.to_owned(), created));
            }
        }
        (200..=206, _) => {
            for id in sent_ids {
                ok.push((id.clone(), created));
            }
        }
        (207, Value::Object(o)) => {
            if let Some(Value::Array(a)) = o.get("success") {
                for id in a.iter().filter_map(Value::as_str).filter(|i| mine(i)) {
                    ok.push((id.to_owned(), created));
                }
            }
            if let Some(Value::Array(a)) = o.get("errors") {
                for e in a {
                    let Some(id) = e
                        .get("entityId")
                        .and_then(Value::as_str)
                        .filter(|i| mine(i))
                    else {
                        continue;
                    };
                    let remote = remote_status(e, status);
                    err.push(err_remote(
                        Some(id),
                        remote,
                        "forwarded batch operation reported an error for this entity",
                    ));
                }
            }
        }
        _ => {
            for id in sent_ids {
                err.push(err_remote(
                    Some(id),
                    status,
                    &format!("forwarded batch operation returned {status}"),
                ));
            }
        }
    }
}

fn ngsi_of(e: ApiError) -> NgsiError {
    match e {
        ApiError::Ngsi(n) => n,
        ApiError::Bare(code) => NgsiError::BadRequestData(format!("HTTP {code}")),
        ApiError::NotAcceptable(_) => NgsiError::BadRequestData("HTTP 406".into()),
        // One item of a batch cannot answer 503: the array's other entries
        // may have succeeded, and 5.6.7.4 gives each failure its own
        // ProblemDetails. The item carries the same detail the whole-request
        // answer would, so the cause is not lost in the collapse.
        ApiError::Overloaded(_) => {
            NgsiError::InternalError(antares_model::error::DB_OVERLOADED.into())
        }
        // The policy gate runs once per request, before the array is
        // touched, so a refusal is the whole array's 403 and never one
        // entry's — this arm is exhaustiveness, not a path. If a per-item
        // gate is ever added, 422 is the nearest thing Table 6.3.2-1 has:
        // the entry was refused, and the reason travels with it.
        ApiError::Denied(why) => NgsiError::OperationNotSupported(why),
    }
}

// ---------- POST /entityOperations/create (5.6.7) ----------

pub async fn batch_create(
    State(st): State<AppState>,
    CleanParams(params): CleanParams,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    match batch_write(&st, &params, &headers, &body, BatchMode::Create).await {
        Ok(r) => r,
        Err(e) => e.into_response(),
    }
}

pub async fn batch_upsert(
    State(st): State<AppState>,
    CleanParams(params): CleanParams,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    match batch_write(&st, &params, &headers, &body, BatchMode::Upsert).await {
        Ok(r) => r,
        Err(e) => e.into_response(),
    }
}

pub async fn batch_update(
    State(st): State<AppState>,
    CleanParams(params): CleanParams,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    match batch_write(&st, &params, &headers, &body, BatchMode::Update).await {
        Ok(r) => r,
        Err(e) => e.into_response(),
    }
}

/// 5.6.20 Batch Entity Merge: each entity merged per 5.6.17 locally;
/// 204 when all succeed, 207 with S/E arrays otherwise (5.6.20.5).
pub async fn batch_merge(
    State(st): State<AppState>,
    CleanParams(params): CleanParams,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    match batch_write(&st, &params, &headers, &body, BatchMode::Merge).await {
        Ok(r) => r,
        Err(e) => e.into_response(),
    }
}

#[derive(Clone, Copy, PartialEq)]
enum BatchMode {
    Create,
    Upsert,
    Update,
    Merge,
}

impl BatchMode {
    /// The clause the policy seam is asked about: one function serves the
    /// four batch write operations, and they are four clauses.
    const fn clause(self) -> &'static str {
        match self {
            Self::Create => "5.6.7",
            Self::Upsert => "5.6.8",
            Self::Update => "5.6.9",
            Self::Merge => "5.6.20",
        }
    }
}

/// The registration-matching input of a batch (4.3.6.1): the ids, the
/// expanded types and the expanded Attribute names the whole array carries,
/// as one `CsrSpec`, together with the items in the shape a forward sends
/// them. An entry whose body is not an object, or whose `@context` did not
/// resolve, contributes nothing and is left to the local arm to report.
fn batch_spec(items: &[BatchItem]) -> (crate::registry::CsrSpec, Vec<FwdItem>) {
    let mut fwd_items = Vec::new();
    let mut spec = crate::registry::CsrSpec::default();
    let (mut types, mut ids, mut attrs) = (Vec::new(), Vec::new(), Vec::new());
    for (item, ctx) in items {
        let (Some(o), Ok(c)) = (item.as_object(), ctx.as_ref()) else {
            continue;
        };
        if let Some(id) = o.get("id").and_then(Value::as_str) {
            ids.push(id.to_owned());
        }
        match o.get("type") {
            Some(Value::String(t)) => types.push(c.expand_key(t)),
            Some(Value::Array(a)) => {
                types.extend(a.iter().filter_map(Value::as_str).map(|t| c.expand_key(t)))
            }
            _ => {}
        }
        for k in o.keys() {
            if !matches!(k.as_str(), "id" | "type" | "scope" | "@context") {
                attrs.push(c.expand_key(k));
            }
        }
        fwd_items.push((o.clone(), c.clone()));
    }
    if !types.is_empty() {
        spec.types = Some(types);
    }
    if !ids.is_empty() {
        spec.ids = Some(ids);
    }
    if !attrs.is_empty() {
        spec.attrs = Some(attrs);
    }
    (spec, fwd_items)
}

/// The distributed arm of a batch write (5.6.7.4, 5.6.8.4, 5.6.9.4,
/// 5.6.20.4): one forwarded request per matching registration, whose
/// outcomes merge into the same S and E arrays the local arm fills, as
/// Entity ids and BatchEntityErrors (5.2.16, 5.2.17) rather than opaque
/// part descriptors.
#[allow(clippy::too_many_arguments)]
async fn forward_batch(
    st: &AppState,
    params: &HashMap<String, String>,
    headers: &HeaderMap,
    mode: BatchMode,
    tenant: &antares_model::TenantId,
    update_mode: bool,
    no_overwrite: bool,
    fed_regs: &[crate::federation::FedReg],
    fwd_items: &[FwdItem],
    out: &mut BatchOutcome,
    created_ids: &mut Vec<String>,
    any_created: &mut bool,
) {
    fn one_outcome(
        id: &str,
        status: u16,
        created: bool,
        ok: &mut Vec<(String, bool)>,
        err: &mut Vec<Value>,
    ) {
        if (200..300).contains(&status) && status != 207 {
            ok.push((id.to_owned(), created && status == 201));
        } else {
            err.push(err_remote(
                Some(id),
                status,
                &format!("forwarded operation returned {status}"),
            ));
        }
    }
    let (op, res_path) = match mode {
        BatchMode::Create => ("createBatch", "create"),
        BatchMode::Upsert => ("upsertBatch", "upsert"),
        BatchMode::Update => ("updateBatch", "update"),
        BatchMode::Merge => ("mergeBatch", "merge"),
    };
    let src = fwd_items
        .first()
        .map(|(_, c)| c.source.clone())
        .unwrap_or(Value::Null);
    let ctx_url = crate::federation::ctx_link_url(headers, &src);
    let mut query: Vec<(String, String)> = Vec::new();
    if let Some(o) = params.get("options") {
        query.push(("options".into(), o.clone()));
    }
    let replace_mode = !update_mode;
    let mut remote_ok: Vec<(String, bool)> = Vec::new();
    let mut remote_err: Vec<Value> = Vec::new();
    for reg in fed_regs {
        let arr: Vec<Value> = fwd_items
            .iter()
            .filter_map(|(o, c)| crate::federation::reduce_to_scope(o, reg, c))
            .collect();
        if arr.is_empty() {
            continue;
        }
        let sent_ids: Vec<String> = arr
            .iter()
            .filter_map(|e| e.get("id").and_then(Value::as_str).map(str::to_owned))
            .collect();
        if reg.supports(op) {
            let (status, body, _) = crate::federation::forward(
                st,
                reqwest::Method::POST,
                format!("{}/ngsi-ld/v1/entityOperations/{res_path}", reg.endpoint),
                &query,
                headers,
                tenant,
                reg,
                &ctx_url,
                Some(Value::Array(arr)),
            )
            .await;
            // 5.6.8.5: an upsert forward only CREATED entities when the
            // remote said so — 201 (body lists the created ids); a 204
            // means every forwarded entity was updated.
            merge_remote_batch(
                status,
                &body,
                &sent_ids,
                mode == BatchMode::Create || (mode == BatchMode::Upsert && status == 201),
                &mut remote_ok,
                &mut remote_err,
            );
            continue;
        }
        // 5.6.7.4/5.6.8.4/5.6.9.4/5.6.20.4 single-op fallbacks. These
        // never inherit the batch `options` parameter — e.g.
        // options=update is no Create Entity parameter (5.6.1.3) and
        // would 400 the forward; the append fallback re-adds
        // noOverwrite explicitly (5.6.9.4).
        let mut handled = true;
        match mode {
            BatchMode::Create if reg.supports("createEntity") => {
                let call = BatchFwd::create();
                for ent in arr.clone() {
                    let (id, status) =
                        forward_one(st, reg, headers, tenant, &ctx_url, ent, &call).await;
                    one_outcome(&id, status, call.created, &mut remote_ok, &mut remote_err);
                }
            }
            BatchMode::Upsert if reg.supports("createEntity") => {
                let create = BatchFwd::create();
                for ent in arr.clone() {
                    let (id, status) =
                        forward_one(st, reg, headers, tenant, &ctx_url, ent.clone(), &create).await;
                    // 5.6.8.4: an Entity the peer already holds is not a
                    // failed upsert — it falls through to the operation
                    // that updates the one that is there.
                    let fallback = match () {
                        _ if status != 409 => None,
                        _ if replace_mode && reg.supports("replaceEntity") => {
                            Some(BatchFwd::replace())
                        }
                        _ if !replace_mode && reg.supports("updateEntity") => {
                            Some(BatchFwd::update())
                        }
                        _ => {
                            // 5.6.8.4: neither replace nor update available
                            remote_err.push(err_remote(
                                Some(&id),
                                422,
                                &format!("OperationNotSupported: no upsert path for {id}"),
                            ));
                            continue;
                        }
                    };
                    match fallback {
                        None => one_outcome(&id, status, true, &mut remote_ok, &mut remote_err),
                        Some(call) => {
                            let (id, status) =
                                forward_one(st, reg, headers, tenant, &ctx_url, ent, &call).await;
                            one_outcome(&id, status, call.created, &mut remote_ok, &mut remote_err);
                        }
                    }
                }
            }
            BatchMode::Upsert if replace_mode && reg.supports("replaceEntity") => {
                let call = BatchFwd::replace();
                for ent in arr.clone() {
                    let (id, status) =
                        forward_one(st, reg, headers, tenant, &ctx_url, ent, &call).await;
                    one_outcome(&id, status, call.created, &mut remote_ok, &mut remote_err);
                }
            }
            BatchMode::Upsert if !replace_mode && reg.supports("updateEntity") => {
                let call = BatchFwd::update();
                for ent in arr.clone() {
                    let (id, status) =
                        forward_one(st, reg, headers, tenant, &ctx_url, ent, &call).await;
                    one_outcome(&id, status, call.created, &mut remote_ok, &mut remote_err);
                }
            }
            BatchMode::Update if !no_overwrite && reg.supports("updateEntity") => {
                let call = BatchFwd::update();
                for ent in arr.clone() {
                    let (id, status) =
                        forward_one(st, reg, headers, tenant, &ctx_url, ent, &call).await;
                    one_outcome(&id, status, call.created, &mut remote_ok, &mut remote_err);
                }
            }
            BatchMode::Merge if reg.supports("mergeEntity") => {
                // 5.6.20.4 support ladder: no mergeBatch -> per-entity
                // Merge Entity (5.6.17) forwards.
                let call = BatchFwd::merge();
                for ent in arr.clone() {
                    let (id, status) =
                        forward_one(st, reg, headers, tenant, &ctx_url, ent, &call).await;
                    one_outcome(&id, status, call.created, &mut remote_ok, &mut remote_err);
                }
            }
            BatchMode::Update if no_overwrite && reg.supports("appendAttrs") => {
                // 5.6.9.4: append with Attribute overwrite disabled.
                let call = BatchFwd::append_no_overwrite();
                for ent in arr.clone() {
                    let (id, status) =
                        forward_one(st, reg, headers, tenant, &ctx_url, ent, &call).await;
                    one_outcome(&id, status, call.created, &mut remote_ok, &mut remote_err);
                }
            }
            _ => handled = false,
        }
        if !handled && reg.is_proxy() {
            // 5.6.7.4/5.6.8.4/5.6.9.4/5.6.20.4 last rung: "In case CSR is
            // an exclusive or redirect Context Source Registration, add an
            // Error of type Conflict for each Entity in IN to E."
            for id in &sent_ids {
                remote_err.push(err_entry(
                    Some(id),
                    &NgsiError::Conflict(format!(
                        "registration does not accept the operation {op}"
                    )),
                ));
            }
        }
    }
    for (id, was_created) in remote_ok {
        if was_created {
            *any_created = true;
            // 5.6.8.4: one Entity ID per Entity, the local and the forwarded
            // part of one Entity included
            if !created_ids.contains(&id) {
                created_ids.push(id.clone());
            }
        }
        if !out.success.iter().any(|v| v.as_str() == Some(id.as_str())) {
            out.success.push(Value::String(id));
        }
    }
    out.errors.extend(remote_err);
}

async fn batch_write(
    st: &AppState,
    params: &HashMap<String, String>,
    headers: &HeaderMap,
    body: &[u8],
    mode: BatchMode,
) -> ApiResult<Response> {
    let tenant = tenant_from(headers)?;
    check_params(params, &["options", "local"])?;
    // T-4562: a batch runs only inside the in-flight budget, held until it answers.
    let _admitted = crate::bounds::admit_batch(body.len())
        .await
        .ok_or(ApiError::Overloaded(crate::negotiate::RETRY_AFTER_SECONDS))?;
    // 6.3.7: `options` is a comma separated list of strings, so a mode is
    // selected when it appears as ONE MEMBER of the list — never by
    // whole-string equality. 6.15.3.1 replace (default) | update for
    // upsert; 6.16.3.1 noOverwrite for update.
    let has_option = |name: &str| {
        params
            .get("options")
            .is_some_and(|o| o.split(',').any(|s| s.trim() == name))
    };
    let update_mode = has_option("update");
    let no_overwrite = has_option("noOverwrite");
    let mut items = parse_batch(st, &tenant, headers, body).await?;
    let mut out = BatchOutcome {
        success: vec![],
        errors: vec![],
    };
    // ADR-0020 per member (T-3480): before any item is written and before
    // any forward, the engine decides each member as the single-Entity write
    // would — its id, its types (stored and written), its Attributes and its
    // expanded body. A refused member lands in E with the AccessDenied
    // problem and the rest are applied (207); a batch refused whole is the
    // 403 it always was. An entry that does not expand is shown by its id
    // alone and left to the local arm to report.
    // ADR-0023 rule 7 under an endpoint view: every batch write is decided
    // per member on the stored Entities below; an engine narrowing still
    // refuses it whole, as `gate!` refuses a narrowed write.
    #[cfg(feature = "views")]
    crate::views::with_view(
        st.view.as_deref(),
        mode.clause(),
        Ok(crate::policy::unnarrowed()),
    )?;
    let mut conds = Conditions::default();
    // Entities read before the first write, which a view's Relationship check
    // takes instead of reading them again (T-4085).
    #[cfg(feature = "views")]
    let mut known: HashMap<String, Value> = HashMap::new();
    if let Some(engine) = &st.policy {
        let members: Vec<(Option<String>, Option<Value>)> = items
            .iter()
            .map(|(item, ctx)| {
                let id = item.get("id").and_then(Value::as_str).map(str::to_owned);
                let body = match (item.as_object(), ctx.as_ref()) {
                    (Some(o), Ok(c)) => expand_entity(o, c, ExpandOpts::default()).ok(),
                    _ => None,
                };
                (id, body)
            })
            .collect();
        // One read for every member's pre-image and, under a view, every
        // Relationship target the batch names (T-4085); a store failure reads
        // as absent, as the per-member read did.
        let mut wanted: Vec<&str> = members.iter().filter_map(|(id, _)| id.as_deref()).collect();
        #[cfg(feature = "views")]
        if st.view.is_some() {
            wanted.extend(
                members
                    .iter()
                    .filter_map(|(_, body)| body.as_ref())
                    .flat_map(crate::views::relationship_targets),
            );
        }
        wanted.sort_unstable();
        wanted.dedup();
        let stored = if wanted.is_empty() {
            HashMap::new()
        } else {
            st.store
                .get_entities(&tenant, &wanted)
                .await
                .unwrap_or_default()
        };
        let mut shown: Vec<(Option<String>, crate::policy::PreImage, Option<Value>)> =
            Vec::with_capacity(members.len());
        for (id, body) in members {
            let pre = match &id {
                Some(id) => crate::policy::image(stored.get(id).cloned(), body.as_ref()),
                None => crate::policy::PreImage::default(),
            };
            shown.push((id, pre, body));
        }
        // What the batch writes itself is read again by the round that needs it.
        #[cfg(feature = "views")]
        {
            let written: std::collections::HashSet<&str> = shown
                .iter()
                .filter_map(|(id, _, _)| id.as_deref())
                .collect();
            known = stored
                .into_iter()
                .filter(|(id, _)| !written.contains(id.as_str()))
                .collect();
        }
        let attrs: Vec<Vec<String>> = shown
            .iter()
            .map(|(_, _, b)| {
                b.as_ref()
                    .and_then(Value::as_object)
                    .map(|o| {
                        o.keys()
                            .filter(|k| !matches!(k.as_str(), "id" | "type" | "scope" | "@context"))
                            .cloned()
                            .collect()
                    })
                    .unwrap_or_default()
            })
            .collect();
        let ids: Vec<Vec<&str>> = shown
            .iter()
            .map(|(id, _, _)| id.as_deref().into_iter().collect())
            .collect();
        let ops: Vec<crate::policy::Operation<'_>> = shown
            .iter()
            .zip(&ids)
            .zip(&attrs)
            .map(|(((_, pre, body), ids), attrs)| crate::policy::Operation {
                ids,
                types: &pre.types,
                attrs,
                body: body.as_ref(),
                stored: pre.stored.as_ref(),
                ..crate::policy::Operation::new(mode.clause())
            })
            .collect();
        let asking = crate::snapshots::asking_tenant(st, &tenant).await;
        let refused = crate::policy::gate_each(engine.as_ref(), &asking, headers, &ops).await;
        drop(ops);
        if let Some(why) = whole_refusal(&refused) {
            return Err(crate::policy::Denied(why).into());
        }
        let mut verdicts = refused.into_iter().zip(shown);
        items.retain(|_| match verdicts.next() {
            Some((Err(why), (id, _, _))) => {
                out.errors.push(denied_entry(id.as_deref(), &why));
                false
            }
            Some((Ok(granted), (id, _, _))) => {
                conds.add(id.as_deref(), &granted);
                true
            }
            None => true,
        });
    }
    let (spec, fwd_items) = batch_spec(&items);
    let fed_regs =
        match crate::federation::write_plan(st, &tenant, &spec, &st.loader.core(), params, headers)
            .await?
        {
            crate::federation::WritePlan::Answered(r) => return Ok(*r),
            crate::federation::WritePlan::Forward(regs) => regs,
        };
    conds.unforwarded(!fed_regs.is_empty())?;
    let mut created_ids: Vec<String> = vec![];
    let mut any_created = false;
    let proxies: Vec<&crate::federation::FedReg> =
        fed_regs.iter().filter(|r| r.is_proxy()).collect();
    // Creates are collected and written as ONE multi-row statement;
    // upsert/update/merge run batched per round below.
    // 5.5.11.0: "All Entities and Attributes in the batch will get the same
    // modifiedAt timestamp" — the clock is read once for the whole array, not
    // per document. Stamping per document spreads a large create over several
    // milliseconds, and a Context Consumer filtering or paging on modifiedAt
    // then sees one batch as two. The per-round stamp below is the separate
    // case: repeated instances of one id are sequential operations (5.5.11.2,
    // 5.5.11.5) and do carry their own instants.
    let create_ts = now_iso();
    let mut pending_creates: Vec<(String, Value)> = Vec::new();
    let mut prepped: Vec<(String, Value)> = Vec::new();
    for (item, ctx) in items {
        let id_hint = item.get("id").and_then(Value::as_str).map(str::to_owned);
        let ctx = match ctx {
            Ok(c) => c,
            Err(e) => {
                out.errors.push(err_entry(id_hint.as_deref(), &ngsi_of(e)));
                continue;
            }
        };
        // proxied (exclusive/redirect) attributes are never stored locally
        let item = if proxies.is_empty() {
            item
        } else if let Some(o) = item.as_object() {
            let (rest, has_attrs) = crate::federation::strip_proxied(o, &proxies, &ctx);
            if !has_attrs {
                continue; // wholly proxied: no local part for this item
            }
            Value::Object(rest)
        } else {
            item
        };
        if mode == BatchMode::Create {
            let prep = || -> Result<(String, Value), NgsiError> {
                let obj = item
                    .as_object()
                    .ok_or_else(|| NgsiError::BadRequestData("entity must be an object".into()))?;
                let mut expanded = expand_entity(obj, &ctx, ExpandOpts::default())?;
                let id = antares_jsonld::expanded_id(&expanded)?.to_owned();
                stamp_new(&mut expanded, &create_ts);
                Ok((id, expanded))
            };
            match prep() {
                Ok(pair) => pending_creates.push(pair),
                Err(e) => out.errors.push(err_entry(id_hint.as_deref(), &e)),
            }
            continue;
        }
        // Expansion/validation per item only;
        // the store operations run BATCHED below — one transaction per round
        // instead of one per item.
        let prep = || -> Result<(String, Value), NgsiError> {
            let obj = item
                .as_object()
                .ok_or_else(|| NgsiError::BadRequestData("entity must be an object".into()))?;
            let fragment_ok = mode == BatchMode::Merge;
            let expanded = expand_entity(
                obj,
                &ctx,
                ExpandOpts {
                    fragment: false,
                    allow_null: fragment_ok,
                    merge: fragment_ok,
                    temporal: false,
                    ..Default::default()
                },
            )?;
            let id = antares_jsonld::expanded_id(&expanded)?.to_owned();
            Ok((id, expanded))
        };
        match prep() {
            Ok(pair) => prepped.push(pair),
            Err(e) => out.errors.push(err_entry(id_hint.as_deref(), &e)),
        }
    }
    // 4.6.6: duplicate instances of one Entity in a batch array "shall come
    // in chronological order" — first oldest. Sequential semantics are kept
    // by splitting into rounds: the Nth occurrence of an id lands in round
    // N, rounds execute in order, and within a round every id is unique.
    let mut rounds: Vec<Vec<(String, Value)>> = Vec::new();
    {
        let mut occurrence: HashMap<String, usize> = HashMap::new();
        for (id, doc) in prepped {
            let n = occurrence.entry(id.clone()).or_insert(0);
            // `n` never passes `rounds.len()`: each id's count grows by one
            // and a round is added the first time a count reaches it
            match rounds.get_mut(*n) {
                Some(round) => round.push((id, doc)),
                None => rounds.push(vec![(id, doc)]),
            }
            *n = n.saturating_add(1);
        }
    }
    for round in rounds {
        let ts = now_iso();
        match mode {
            // Creates are collected into pending_creates above and never
            // reach a round; a request handler answers, it does not panic.
            BatchMode::Create => {
                for (id, _) in round {
                    out.errors.push(err_entry(
                        Some(&id),
                        &NgsiError::InternalError(
                            "batch create is handled before the rounds".into(),
                        ),
                    ));
                }
            }
            BatchMode::Upsert => {
                #[cfg(feature = "views")]
                if st.view.is_some() {
                    for id in upsert_in_view(
                        st,
                        &tenant,
                        round,
                        update_mode,
                        &ts,
                        &conds,
                        &known,
                        &mut out,
                    )
                    .await?
                    {
                        any_created = true;
                        if !created_ids.contains(&id) {
                            created_ids.push(id);
                        }
                    }
                    continue;
                }
                // options=update: merge into existing rows first; ids that
                // turn out absent (or vanish mid-flight) fall through to the
                // replace batch — never a silent success (TOCTOU fix).
                let mut replaces: Vec<(String, Value)> = Vec::new();
                if update_mode {
                    let ids: Vec<String> = round.iter().map(|(id, _)| id.clone()).collect();
                    let docs: HashMap<&str, &Value> =
                        round.iter().map(|(id, d)| (id.as_str(), d)).collect();
                    let res = st
                        .store
                        .batch_mutate(&tenant, &ids, |id, doc| {
                            conds.pre(id, doc)?;
                            let Some(&src) = docs.get(id) else {
                                return Err(NgsiError::InternalError(format!(
                                    "batch member {id} lost its document"
                                )));
                            };
                            let before = doc.clone();
                            merge_into(doc, src, &ts);
                            // a member that only restates the stored state
                            // (a parked vehicle, a poll that saw nothing
                            // new) is no change (4.8, 5.8.3): handing the
                            // store the document unchanged writes no row,
                            // version, history point or notification
                            if same_but_stamps(&before, doc) {
                                *doc = before;
                            }
                            Ok::<(), NgsiError>(())
                        })
                        .await?;
                    for ((id, expanded), r) in round.iter().zip(res) {
                        match r {
                            Some(Err(e)) => out.errors.push(err_entry(Some(id), &e)),
                            Some(Ok(())) => {
                                if !out.success.contains(&Value::String(id.clone())) {
                                    out.success.push(Value::String(id.clone()));
                                }
                            }
                            None if conds.has(id) => out
                                .errors
                                .push(err_entry(Some(id), &Conditions::not_found(id))),
                            None => replaces.push((id.clone(), expanded.clone())),
                        }
                    }
                } else {
                    // a conditioned member is replaced only inside the
                    // store's read of it, where its condition is judged
                    let (conditioned, free): (Vec<_>, Vec<_>) =
                        round.into_iter().partition(|(id, _)| conds.has(id));
                    replaces = free;
                    if !conditioned.is_empty() {
                        let ids: Vec<String> =
                            conditioned.iter().map(|(id, _)| id.clone()).collect();
                        let docs: HashMap<&str, &Value> =
                            conditioned.iter().map(|(id, d)| (id.as_str(), d)).collect();
                        let res = st
                            .store
                            .batch_mutate(&tenant, &ids, |id, doc| {
                                conds.pre(id, doc)?;
                                let Some(&src) = docs.get(id) else {
                                    return Err(NgsiError::InternalError(format!(
                                        "batch member {id} lost its document"
                                    )));
                                };
                                let mut new = src.clone();
                                stamp_new(&mut new, &ts);
                                // 4.8: a replaced Entity keeps the instant it was created.
                                if let (Some(o), Some(created)) =
                                    (new.as_object_mut(), doc.get("createdAt"))
                                {
                                    o.insert("createdAt".into(), created.clone());
                                }
                                *doc = new;
                                Ok::<(), NgsiError>(())
                            })
                            .await?;
                        for (id, r) in ids.iter().zip(res) {
                            match r {
                                Some(Ok(())) => {
                                    if !out.success.contains(&Value::String(id.clone())) {
                                        out.success.push(Value::String(id.clone()));
                                    }
                                }
                                Some(Err(e)) => out.errors.push(err_entry(Some(id), &e)),
                                None => out
                                    .errors
                                    .push(err_entry(Some(id), &Conditions::not_found(id))),
                            }
                        }
                    }
                }
                if !replaces.is_empty() {
                    for (_, doc) in replaces.iter_mut() {
                        stamp_new(doc, &ts);
                    }
                    // ids first, then MOVE the documents into the store —
                    // the loop below only needs the ids, so the batch payload
                    // is never deep-cloned on the ingest hot path.
                    let ids: Vec<String> = replaces.iter().map(|(id, _)| id.clone()).collect();
                    let flags = st.store.batch_upsert(&tenant, replaces).await?;
                    for (id, created) in ids.iter().zip(flags) {
                        if created {
                            any_created = true;
                            if !created_ids.contains(id) {
                                created_ids.push(id.clone());
                            }
                        }
                        if !out.success.contains(&Value::String(id.clone())) {
                            out.success.push(Value::String(id.clone()));
                        }
                    }
                }
            }
            BatchMode::Update | BatchMode::Merge => {
                #[cfg(feature = "views")]
                let (round, guards) = view_guards(st, &tenant, round, &known, &mut out).await;
                let ids: Vec<String> = round.iter().map(|(id, _)| id.clone()).collect();
                let docs: HashMap<&str, &Value> =
                    round.iter().map(|(id, d)| (id.as_str(), d)).collect();
                // batch update with noOverwrite: existing attribute instances
                // are left alone; if any existed, the entity is a partial
                // failure (005_02 ⇒ 207)
                let mut skipped: HashMap<String, bool> = HashMap::new();
                let res = st
                    .store
                    .batch_mutate(&tenant, &ids, |id, doc| {
                        conds.pre(id, doc)?;
                        #[cfg(feature = "views")]
                        let guard = guards.get(id);
                        #[cfg(feature = "views")]
                        if let Some(g) = guard {
                            g.pre(id, doc)?;
                        }
                        let Some(&expanded) = docs.get(id) else {
                            return Err(NgsiError::InternalError(format!(
                                "batch member {id} lost its document"
                            )));
                        };
                        if mode == BatchMode::Update && no_overwrite {
                            // noOverwrite is instance-level: only instances
                            // whose datasetId already exists are skipped
                            let target = antares_store::stored_object(doc)?;
                            for (k, v) in antares_jsonld::expanded_object(expanded)? {
                                if matches!(
                                    k.as_str(),
                                    "id" | "type" | "scope" | "createdAt" | "modifiedAt"
                                ) {
                                    continue;
                                }
                                let incoming: Vec<Value> =
                                    v.as_array().cloned().unwrap_or_default();
                                match target.get_mut(k).and_then(Value::as_array_mut) {
                                    None => {
                                        target.insert(k.clone(), Value::Array(incoming));
                                    }
                                    Some(cur) => {
                                        for ni in incoming {
                                            let ds = ni.get("datasetId").and_then(Value::as_str);
                                            if cur.iter().any(|ci| {
                                                ci.get("datasetId").and_then(Value::as_str) == ds
                                            }) {
                                                skipped.insert(id.to_owned(), true);
                                            } else {
                                                cur.push(ni);
                                            }
                                        }
                                    }
                                }
                            }
                            target.insert("modifiedAt".into(), Value::String(ts.clone()));
                        } else {
                            merge_into(doc, expanded, &ts);
                        }
                        #[cfg(feature = "views")]
                        if let Some(g) = guard {
                            g.post(doc)?;
                        }
                        Ok::<(), NgsiError>(())
                    })
                    .await?;
                for ((id, _), r) in round.iter().zip(res) {
                    #[cfg(feature = "views")]
                    if let Some(crate::policy::Denied(why)) =
                        guards.get(id).and_then(|g| g.verdict().err())
                    {
                        out.errors.push(denied_entry(Some(id), &why));
                        continue;
                    }
                    match r {
                        None => out.errors.push(err_entry(
                            Some(id),
                            &NgsiError::ResourceNotFound(format!("entity {id} not found")),
                        )),
                        Some(Err(e)) => out.errors.push(err_entry(Some(id), &e)),
                        Some(Ok(())) => {
                            if skipped.get(id).copied().unwrap_or(false) {
                                out.errors.push(err_entry(
                                    Some(id),
                                    &NgsiError::BadRequestData(
                                        "some attributes already existed (noOverwrite)".into(),
                                    ),
                                ));
                            } else if !out.success.contains(&Value::String(id.clone())) {
                                out.success.push(Value::String(id.clone()));
                            }
                        }
                    }
                }
            }
        }
    }
    // ADR-0023 rule 7 per member: a create through a view lands in it.
    #[cfg(feature = "views")]
    if st.view.is_some() {
        let mut kept = Vec::with_capacity(pending_creates.len());
        for (id, doc) in pending_creates {
            match crate::views::admits_new(st, &tenant, &doc).await {
                Ok(()) => kept.push((id, doc)),
                Err(crate::policy::Denied(why)) => out.errors.push(denied_entry(Some(&id), &why)),
            }
        }
        pending_creates = kept;
    }
    // The collected creates, one multi-row statement, one transaction.
    if !pending_creates.is_empty() {
        let ids: Vec<String> = pending_creates.iter().map(|(id, _)| id.clone()).collect();
        let flags = st.store.batch_create(&tenant, pending_creates).await?;
        for (id, created) in ids.iter().zip(flags) {
            if created {
                any_created = true;
                if !created_ids.contains(id) {
                    created_ids.push(id.clone());
                }
                if !out.success.contains(&Value::String(id.clone())) {
                    out.success.push(Value::String(id.clone()));
                }
            } else {
                out.errors.push(err_entry(
                    Some(id),
                    &NgsiError::AlreadyExists(format!("entity {id} already exists")),
                ));
            }
        }
    }
    // Distributed arm (5.6.7.4/5.6.8.4/5.6.9.4/5.6.20.4): remote outcomes
    // merge into the same S/E arrays as local ones — the response body
    // carries Entity IDs and BatchEntityErrors (5.2.16/5.2.17), never
    // opaque part descriptors.
    if !fed_regs.is_empty() {
        forward_batch(
            st,
            params,
            headers,
            mode,
            &tenant,
            update_mode,
            no_overwrite,
            &fed_regs,
            &fwd_items,
            &mut out,
            &mut created_ids,
            &mut any_created,
        )
        .await;
    }
    // 5.6.8.5: the created-only S array is the ALL-SUCCEEDED reading ("if all
    // Entities not existing prior to this request have been successfully
    // created and the others have been successfully updated"). Once E is
    // non-empty the third bullet applies and S is "the list of Entities
    // successfully created or updated".
    if mode == BatchMode::Upsert && any_created && out.errors.is_empty() {
        out.success = created_ids.into_iter().map(Value::String).collect();
    }
    let (status, body_on_ok) = match mode {
        BatchMode::Create => (StatusCode::CREATED, true),
        BatchMode::Upsert => {
            if any_created {
                (StatusCode::CREATED, true)
            } else {
                (StatusCode::NO_CONTENT, false)
            }
        }
        BatchMode::Update | BatchMode::Merge => (StatusCode::NO_CONTENT, false),
    };
    Ok(out.respond(&tenant, status, body_on_ok))
}

/// Where a forwarded batch item lands on the peer.
#[derive(Clone, Copy)]
enum FwdPath {
    Collection,
    Entity,
    Attrs,
}

/// The single-Entity request a batch operation forwards when the peer does
/// not support the batch operation itself — the 5.6.7.4 / 5.6.8.4 / 5.6.9.4 /
/// 5.6.20.4 fallback ladders. 5.6.7 posts to the collection, 5.6.8 replaces
/// the Entity or updates its Attributes, 5.6.9 updates them or appends with
/// overwrite disabled, 5.6.20 merges the Entity; the loop around them is the
/// same. None of them inherits the batch `options` parameter, which is no
/// parameter of the single-Entity operation being carried — 5.6.9.4's
/// noOverwrite is re-added here, explicitly, because that one is.
struct BatchFwd {
    path: FwdPath,
    method: reqwest::Method,
    query: Vec<(String, String)>,
    /// whether a 201 from the peer counts as a creation in the 5.2.16 S array
    created: bool,
}

impl BatchFwd {
    /// 5.6.1 Create Entity.
    fn create() -> Self {
        Self {
            path: FwdPath::Collection,
            method: reqwest::Method::POST,
            query: Vec::new(),
            created: true,
        }
    }
    /// 5.6.4 Replace Entity.
    fn replace() -> Self {
        Self {
            path: FwdPath::Entity,
            method: reqwest::Method::PUT,
            query: Vec::new(),
            created: false,
        }
    }
    /// 5.6.2 Update Entity Attributes.
    fn update() -> Self {
        Self {
            path: FwdPath::Attrs,
            method: reqwest::Method::PATCH,
            query: Vec::new(),
            created: false,
        }
    }
    /// 5.6.17 Merge Entity.
    fn merge() -> Self {
        Self {
            path: FwdPath::Entity,
            method: reqwest::Method::PATCH,
            query: Vec::new(),
            created: false,
        }
    }
    /// 5.6.3 Append Entity Attributes with overwrite disabled (5.6.9.4).
    fn append_no_overwrite() -> Self {
        Self {
            path: FwdPath::Attrs,
            method: reqwest::Method::POST,
            query: vec![("options".into(), "noOverwrite".into())],
            created: false,
        }
    }
}

/// Forward one Entity of the batch to one registration (4.3.6.3) and report
/// the id it was sent under with the status the peer answered. The id is
/// reported back to the client verbatim, so it is kept raw here and
/// percent-encoded per RFC 3986 clause 3.3 only where it becomes a path
/// segment of the forwarded URL.
async fn forward_one(
    st: &AppState,
    reg: &crate::federation::FedReg,
    headers: &HeaderMap,
    tenant: &antares_model::TenantId,
    ctx_url: &str,
    ent: Value,
    call: &BatchFwd,
) -> (String, u16) {
    let id = ent
        .get("id")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_owned();
    let seg = crate::federation::path_segment(&id);
    let base = &reg.endpoint;
    let url = match call.path {
        FwdPath::Collection => format!("{base}/ngsi-ld/v1/entities"),
        FwdPath::Entity => format!("{base}/ngsi-ld/v1/entities/{seg}"),
        FwdPath::Attrs => format!("{base}/ngsi-ld/v1/entities/{seg}/attrs"),
    };
    let (status, _, _) = crate::federation::forward(
        st,
        call.method.clone(),
        url,
        &call.query,
        headers,
        tenant,
        reg,
        ctx_url,
        Some(ent),
    )
    .await;
    (id, status)
}

// ---------- POST /entityOperations/delete (5.6.10) ----------

pub async fn batch_delete(
    State(st): State<AppState>,
    CleanParams(params): CleanParams,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let go = async {
        let tenant = tenant_from(&headers)?;
        check_params(&params, &["local"])?;
        let ct = content_type(&headers)?;
        if ct != "application/json" && ct != "application/ld+json" {
            return Err(ApiError::Bare(StatusCode::UNSUPPORTED_MEDIA_TYPE));
        }
        let value: Value = serde_json::from_slice(&body)
            .map_err(|e| NgsiError::InvalidRequest(format!("body is not valid JSON: {e}")))?;
        let ids = value.as_array().filter(|a| !a.is_empty()).ok_or_else(|| {
            NgsiError::BadRequestData("batch delete body must be a non-empty array".into())
        })?;
        // 5.6.10.4: a null item fails the whole request
        if ids.iter().any(Value::is_null) {
            return Err(NgsiError::BadRequestData(
                "batch array must not contain null items".into(),
            )
            .into());
        }
        // Batch entity count cap — the same ceiling the write batches carry,
        // and the one that bounds the per-id forwarded DELETE fan-out below.
        if ids.len() > *crate::bounds::MAX_BATCH_ITEMS {
            return Err(NgsiError::BadRequestData(format!(
                "batch of {} exceeds the {}-entity limit",
                ids.len(),
                *crate::bounds::MAX_BATCH_ITEMS
            ))
            .into());
        }
        let mut out = BatchOutcome {
            success: vec![],
            errors: vec![],
        };
        // ADR-0020 per member, as for the write batches (T-3480): each id
        // is decided as the single-Entity delete decides it, with the types
        // stored under it. The body has to be read to know them, so the gate
        // sits behind the parse the way the single-Entity delete sits behind
        // its path parameter. A refused id lands in E, the rest are deleted.
        let mut ids: Vec<&Value> = ids.iter().collect();
        let mut conds = Conditions::default();
        #[cfg(feature = "views")]
        crate::views::with_view(
            st.view.as_deref(),
            "5.6.10",
            Ok(crate::policy::unnarrowed()),
        )?;
        if let Some(engine) = &st.policy {
            let mut pres: Vec<crate::policy::PreImage> = Vec::new();
            for id in &ids {
                pres.push(match id.as_str() {
                    Some(id) => crate::policy::pre_image(&st, &tenant, id, None).await,
                    None => crate::policy::PreImage::default(),
                });
            }
            let one: Vec<Vec<&str>> = ids
                .iter()
                .map(|v| v.as_str().into_iter().collect())
                .collect();
            let ops: Vec<crate::policy::Operation<'_>> = one
                .iter()
                .zip(&pres)
                .map(|(ids, pre)| crate::policy::Operation {
                    ids,
                    types: &pre.types,
                    stored: pre.stored.as_ref(),
                    ..crate::policy::Operation::new("5.6.10")
                })
                .collect();
            let asking = crate::snapshots::asking_tenant(&st, &tenant).await;
            let refused = crate::policy::gate_each(engine.as_ref(), &asking, &headers, &ops).await;
            if let Some(why) = whole_refusal(&refused) {
                return Err(crate::policy::Denied(why).into());
            }
            let mut verdicts = refused.into_iter();
            ids.retain(|id| match verdicts.next() {
                Some(Err(why)) => {
                    out.errors.push(denied_entry(id.as_str(), &why));
                    false
                }
                Some(Ok(granted)) => {
                    conds.add(id.as_str(), &granted);
                    true
                }
                None => true,
            });
        }
        let spec = crate::registry::CsrSpec {
            ids: Some(
                ids.iter()
                    .filter_map(|v| v.as_str())
                    .map(str::to_owned)
                    .collect(),
            ),
            ..Default::default()
        };
        let regs = match crate::federation::write_plan(
            &st,
            &tenant,
            &spec,
            &st.loader.core(),
            &params,
            &headers,
        )
        .await?
        {
            crate::federation::WritePlan::Answered(r) => return Ok(*r),
            crate::federation::WritePlan::Forward(regs) => regs,
        };
        conds.unforwarded(!regs.is_empty())?;
        // One multi-row DELETE for the whole batch; flags in input order.
        let id_strs: Vec<String> = ids
            .iter()
            .copied()
            .filter_map(Value::as_str)
            .map(str::to_owned)
            .collect();
        // Under an endpoint view or an engine's condition each delete reads
        // its Entity under the lock and leaves one out of the view, or one
        // the condition does not select, which then answers as absent
        // (ADR-0023 rule 7, gap G12).
        #[cfg(feature = "views")]
        let guard = crate::views::WriteGuard::of(&st, std::iter::empty());
        #[cfg(feature = "views")]
        let judged = guard.is_some() || !conds.0.is_empty();
        #[cfg(not(feature = "views"))]
        let judged = !conds.0.is_empty();
        let flags = if judged {
            let mut flags = Vec::with_capacity(id_strs.len());
            for id in &id_strs {
                let keep = |d: &Value| {
                    #[cfg(feature = "views")]
                    if guard.as_ref().is_some_and(|g| !g.sees(d)) {
                        return false;
                    }
                    conds.holds(id, d)
                };
                flags.push(st.store.delete_entity_if(&tenant, id, &keep).await?);
            }
            flags
        } else {
            st.store.batch_delete(&tenant, &id_strs).await?
        };
        let mut flags = flags.into_iter();
        let mut local_ok: std::collections::HashSet<String> = Default::default();
        let mut local_miss: Vec<String> = Vec::new();
        for id in ids {
            let Some(id) = id.as_str() else {
                out.errors.push(err_entry(
                    None,
                    &NgsiError::BadRequestData("entity id must be a string".into()),
                ));
                continue;
            };
            if flags.next().unwrap_or(false) {
                // 5.6.10 deletes carry the same temporal-deletion semantics
                // as 5.6.6 — without this, batch-deleted entities live on in
                // the temporal store (the reset's batch delete leaked
                // every prior suite's Buildings into the orderBy queries).
                crate::history::mirror_delete_entity(&st, &tenant, id).await;
                local_ok.insert(id.to_owned());
            } else {
                // proxied entities may live remotely only (4.3.6.3) — a
                // local miss becomes an error only if no forward covers it.
                local_miss.push(id.to_owned());
            }
        }
        // 5.6.10.4 support ladder: deleteBatch -> one batch forward; else
        // per-entity Delete Entity forwards; else proxy modes get Conflict
        // per entity. Remote outcomes merge into S/E (never opaque parts).
        let mut remote_ok: Vec<(String, bool)> = Vec::new();
        let mut remote_err: Vec<Value> = Vec::new();
        if !regs.is_empty() {
            let ctx_url = crate::federation::ctx_link_url(&headers, &st.loader.core().source);
            for reg in &regs {
                // "Remove from IN all Entities not matched by CSR" — an
                // id-scoped registration (exact ids OR idPattern, 5.12)
                // only receives its own ids (4.3.6.1).
                let sent_ids: Vec<String> = id_strs
                    .iter()
                    .filter(|i| reg.can_match_id(i))
                    .cloned()
                    .collect();
                if sent_ids.is_empty() {
                    continue;
                }
                if reg.supports("deleteBatch") {
                    let sent_vals: Vec<Value> =
                        sent_ids.iter().cloned().map(Value::String).collect();
                    let (status, body, _) = crate::federation::forward(
                        &st,
                        reqwest::Method::POST,
                        format!("{}/ngsi-ld/v1/entityOperations/delete", reg.endpoint),
                        &[],
                        &headers,
                        &tenant,
                        reg,
                        &ctx_url,
                        Some(Value::Array(sent_vals)),
                    )
                    .await;
                    merge_remote_batch(
                        status,
                        &body,
                        &sent_ids,
                        false,
                        &mut remote_ok,
                        &mut remote_err,
                    );
                } else if reg.supports("deleteEntity") {
                    for id in &sent_ids {
                        let (status, _, _) = crate::federation::forward(
                            &st,
                            reqwest::Method::DELETE,
                            format!(
                                "{}/ngsi-ld/v1/entities/{}",
                                reg.endpoint,
                                crate::federation::path_segment(id.as_str())
                            ),
                            &[],
                            &headers,
                            &tenant,
                            reg,
                            &ctx_url,
                            None,
                        )
                        .await;
                        if (200..300).contains(&status) && status != 207 {
                            remote_ok.push((id.clone(), false));
                        } else {
                            remote_err.push(err_remote(
                                Some(id),
                                status,
                                &format!("forwarded delete returned {status}"),
                            ));
                        }
                    }
                } else if reg.is_proxy() {
                    // 5.6.10.4 last rung: an exclusive or redirect CSR that
                    // supports neither delete operation contributes an Error
                    // of type Conflict for each Entity in IN.
                    for id in &sent_ids {
                        remote_err.push(err_entry(
                            Some(id),
                            &NgsiError::Conflict(
                                "registration does not accept the operation deleteBatch".into(),
                            ),
                        ));
                    }
                }
            }
        }
        let remote_success: std::collections::HashSet<String> =
            remote_ok.iter().map(|(id, _)| id.clone()).collect();
        // success in input order (local first occurrence), then remote-only
        for id in &id_strs {
            if local_ok.contains(id) && !out.success.iter().any(|v| v.as_str() == Some(id.as_str()))
            {
                out.success.push(Value::String(id.clone()));
            }
        }
        for (id, _) in remote_ok {
            if !out.success.iter().any(|v| v.as_str() == Some(id.as_str())) {
                out.success.push(Value::String(id));
            }
        }
        let erred: Vec<String> = remote_err
            .iter()
            .filter_map(|e| e.get("entityId").and_then(Value::as_str).map(str::to_owned))
            .collect();
        out.errors.extend(remote_err);
        // 5.6.10.4 local step (5.6.6 limited to local): a missed occurrence
        // is ResourceNotFound unless a FORWARD resolved that id — a local
        // success does not excuse it (5.5.11.4 duplicate-id semantics: the
        // second occurrence of the same id errors).
        for id in local_miss {
            if !remote_success.contains(&id) && !erred.contains(&id) {
                out.errors.push(err_entry(
                    Some(&id),
                    &NgsiError::ResourceNotFound(format!("entity {id} not found")),
                ));
            }
        }
        Ok::<_, ApiError>(out.respond(&tenant, StatusCode::NO_CONTENT, false))
    };
    go.await.unwrap_or_else(|e| e.into_response())
}

// ---------- POST /entityOperations/query (6.23) ----------

pub async fn batch_query(
    State(st): State<AppState>,
    CleanParams(params): CleanParams,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    match batch_query_inner(&st, &params, &headers, &body).await {
        Ok(r) => r,
        Err(e) => e.into_response(),
    }
}

async fn batch_query_inner(
    st: &AppState,
    params: &HashMap<String, String>,
    headers: &HeaderMap,
    body: &[u8],
) -> ApiResult<Response> {
    let tenant = tenant_from(headers)?;
    check_params(
        params,
        &["limit", "offset", "count", "options", "format", "local"],
    )?;
    // POST query IS Query Entities: geo+json is a valid Accept here (6.3.15)
    let accept = parse_accept_geo(headers)?;
    let parsed = parse_body(st, headers, body, BodyKind::Standard).await?;
    let q = parsed.object(NgsiError::BadRequestData(
        "query body must be an object".into(),
    ))?;
    if q.get("type").and_then(Value::as_str) != Some("Query") {
        return Err(NgsiError::BadRequestData("body type must be Query (5.2.23)".into()).into());
    }
    // Convert Query members into virtual params reusing the GET filter path.
    let mut vp: HashMap<String, String> = HashMap::new();
    query_doc_params(q, false, &mut vp)?;
    if let Some(l) = params.get("local") {
        vp.insert("local".into(), l.clone());
    }
    // ADR-0020: the engine is shown every member of the Query body
    let named = crate::entities::ReadMembers::with_ctx(st, &vp, &parsed.ctx);
    let filter = gate!(
        st, &tenant, headers, "5.7.2",
        q: named.q.as_ref(),
        scope_q: vp.get("scopeQ").map(String::as_str),
        types: &named.types,
        attrs: &named.attrs,
        geo: named.geo.as_ref(),
    )
    .await?;
    // 5.7.2.4 (p. 201): "At least one of the following input data shall be
    // provided: a) selector of Entity Types; b) list of Attribute names,
    // including at least one non-system Attribute; c) NGSI-LD Query,
    // including at least one non-system Attribute; d) NGSI-LD GeoQuery;
    // e) local scope. If none of the above is provided, then an error of type
    // BadRequestData shall be raised (too wide query)." Query Entities is ONE
    // operation: the resource that carries the Query in a body answers to the
    // same behaviour clause as the one that carries it in the URI, and
    // without this a bare `{"type":"Query"}` reads the whole tenant and fans
    // the filterless query out to every matching registration.
    let q_ast = vp.get("q").map(|q| antares_ql::parse_q(q)).transpose()?;
    // 5.7.2.4: "if ... the query, geoquery or context source filter are not
    // syntactically valid (as per the referred clauses 4.9 and 4.10) an error
    // of type BadRequestData shall be raised." Registration matching parses
    // the csf again and drops what it cannot parse, which widens the fan-out
    // to the Context Sources the filter was there to exclude — so it is
    // refused here, the way the twin carrying the Query in the URI refuses it.
    if let Some(csf) = vp.get("csf") {
        antares_ql::parse_q(csf)?;
    }
    if !crate::entities::qualifies_non_wide(&vp, q_ast.as_ref()) {
        return Err(NgsiError::BadRequestData(
            "query needs at least one of type, attrs, q, georel (5.7.2.4)".into(),
        )
        .into());
    }
    // ADR-0020: the engine's narrowing joins the Query's own filters here,
    // after the "too wide" judgement — which is about what the client asked
    // for. Everything below reads the narrowed query, the forwarded one
    // included.
    // what the client's own Query reads, before the engine's q joins it
    // (ADR-0023 rule 11)
    let narrow = crate::repr::narrowing_reading(&filter, &vp, &parsed.ctx)?;
    let vp = filter.narrow_params(&vp, &parsed.ctx)?;
    let fed = if crate::federation::active(&vp)
        && !crate::federation::via_loop(
            headers,
            &crate::federation::alias_for(&st.host_alias, &tenant),
        ) {
        // 6.3.17 scopes NGSILD-Warning to GET /entities(/{id}) — collected
        // here for the log only, never emitted on entityOperations/query
        let mut warnings = Vec::new();
        let fed =
            crate::federation::fed_query(st, &tenant, headers, &parsed.ctx, &vp, &mut warnings)
                .await?;
        for w in &warnings {
            tracing::debug!("distributed query warning (batch query): {w}");
        }
        fed
    } else {
        Vec::new()
    };
    let mut matches =
        crate::entities::filter_entities_fed(st, &tenant, &vp, &parsed.ctx, fed, Some(&narrow))
            .await?;
    let mut page_params = params.clone();
    page_params.extend(vp.clone());
    // 5.2.43 ordering: same 4.23 keys as the GET twin, applied pre-pagination
    if let Some(spec) = page_params.get("orderBy") {
        crate::repr::order_as_shown(&mut matches, Some(&narrow), |m| {
            crate::paging::order_entities(m, spec, &page_params, &parsed.ctx)
        })?;
    }
    let (page, count_hdr, _links) = paginate(
        st,
        &page_params,
        matches,
        "/ngsi-ld/v1/entityOperations/query",
    )?;
    // body members (pick/omit/attrs/lang/datasetId) shape the representation
    // exactly like their 6.3.7 query-parameter twins
    let mut repr = parse_repr(&page_params, &parsed.ctx)?;
    crate::repr::narrow_repr(&mut repr, &filter);
    let join = crate::entities::parse_join(&vp)?;
    crate::entities::check_linked_projection(&repr, &join)?;
    let mut payload: Vec<Value> = page
        .iter()
        .filter_map(|doc| {
            let shaped = apply(doc, &repr);
            if repr.pick.is_some() && shaped.as_object().is_some_and(|o| o.is_empty()) {
                return None;
            }
            Some(crate::repr::compact_for(&repr, &shaped, &parsed.ctx))
        })
        .collect();
    if let Some((mode, level)) = &join {
        // 4.5.23.1 bounds the WIDTH of the retrieval per REQUEST, so one
        // allowance is spent across the whole page, exactly as the GET twin
        // spends it. Minting a fresh allowance per payload Entity multiplies
        // the ceiling by the page size, which is the request the ceiling
        // exists to bound: a page of densely linked Entities.
        let held = crate::entities::contained_by(&page_params);
        let mut budget = crate::repr::MAX_JOIN_LOOKUPS;
        match mode.as_str() {
            "inline" => {
                for p in &mut payload {
                    crate::repr::inline_join_beyond(
                        st,
                        &tenant,
                        &parsed.ctx,
                        &repr,
                        p,
                        *level,
                        &held,
                        &mut budget,
                        crate::federation::active(&page_params).then_some(headers),
                    )
                    .await;
                }
            }
            "flat" => {
                let mut linked = std::collections::BTreeMap::new();
                for doc in &page {
                    crate::repr::collect_flat_beyond(
                        st,
                        &tenant,
                        &parsed.ctx,
                        &repr,
                        doc,
                        *level,
                        &mut linked,
                        &held,
                        &mut budget,
                    )
                    .await;
                }
                let page_ids: Vec<&str> = page.iter().filter_map(|d| d["id"].as_str()).collect();
                for (id, (ldoc, lrepr)) in linked {
                    if !page_ids.contains(&id.as_str()) {
                        payload.push(crate::repr::compact_for(
                            &lrepr,
                            &apply(&ldoc, &lrepr),
                            &parsed.ctx,
                        ));
                    }
                }
            }
            _ => {}
        }
    }
    let out = if accept == Accept::GeoJson {
        crate::repr::to_geojson_collection(payload, None)
    } else {
        Value::Array(payload)
    };
    // 6.3.6: the json answer's Link names the @context it was compacted in
    let served = crate::contexts::linkable(st, &tenant, headers, &parsed.ctx, accept).await?;
    let mut resp = respond_prefer(StatusCode::OK, out, &served, accept, &tenant, headers);
    if let Some(total) = count_hdr {
        if let Ok(v) = total.to_string().parse() {
            resp.headers_mut().insert("NGSILD-Results-Count", v);
        }
    }
    filter.mark(&st.host_alias, resp.headers_mut());
    Ok(resp)
}

/// `a` and `b` hold the same Entity apart from the `modifiedAt` stamps a
/// merge leaves on the Entity and on each Attribute instance it touched.
/// Only those two levels are skipped: a `modifiedAt` key inside a value
/// is data and compares like any other.
fn same_but_stamps(a: &Value, b: &Value) -> bool {
    fn without(o: &serde_json::Map<String, Value>) -> impl Iterator<Item = (&String, &Value)> {
        o.iter().filter(|(k, _)| k.as_str() != "modifiedAt")
    }
    fn same_obj(
        a: &serde_json::Map<String, Value>,
        b: &serde_json::Map<String, Value>,
        inner: fn(&Value, &Value) -> bool,
    ) -> bool {
        without(a).count() == without(b).count()
            && without(a).all(|(k, v)| b.get(k).is_some_and(|w| inner(v, w)))
    }
    fn instances(a: &Value, b: &Value) -> bool {
        match (a.as_array(), b.as_array()) {
            (Some(x), Some(y)) => {
                x.len() == y.len()
                    && x.iter()
                        .zip(y)
                        .all(|(i, j)| match (i.as_object(), j.as_object()) {
                            (Some(i), Some(j)) => same_obj(i, j, |v, w| v == w),
                            _ => i == j,
                        })
            }
            _ => a == b,
        }
    }
    match (a.as_object(), b.as_object()) {
        (Some(a), Some(b)) => same_obj(a, b, instances),
        _ => a == b,
    }
}

#[cfg(test)]
mod tests {
    /// T-3531: a remote error status is taken only when it is one.
    #[test]
    fn remote_status_takes_only_an_http_error_status() {
        use serde_json::json;
        let e = |s: Value| json!({"error": {"status": s}});
        assert_eq!(super::remote_status(&e(json!(404)), 207), 404);
        assert_eq!(super::remote_status(&e(json!(65_736)), 207), 207);
        assert_eq!(super::remote_status(&e(json!(200)), 207), 207);
        assert_eq!(super::remote_status(&e(json!("404")), 207), 207);
        assert_eq!(super::remote_status(&json!({}), 500), 500);
    }

    use super::*;
    use axum::body::Body;
    use axum::http::Request;
    use axum::Router;
    use http_body_util::BodyExt;
    use tower::ServiceExt;

    fn app() -> Router {
        crate::router(AppState::new("antares-test".into()))
    }

    async fn post(app: &Router, uri: &str, body: Value) -> Response {
        let s = body.to_string();
        app.clone()
            .oneshot(
                Request::post(uri)
                    .header("Content-Type", "application/json")
                    .header("Content-Length", s.len())
                    .body(Body::from(s))
                    .expect("req"),
            )
            .await
            .expect("resp")
    }

    async fn get_status(app: &Router, uri: &str) -> StatusCode {
        app.clone()
            .oneshot(Request::get(uri).body(Body::empty()).expect("req"))
            .await
            .expect("resp")
            .status()
    }

    async fn body_json(resp: Response) -> Value {
        let bytes = resp.into_body().collect().await.expect("body").to_bytes();
        serde_json::from_slice(&bytes).expect("json body")
    }

    /// 5.7.2.4 (p. 201): "At least one of the following input data shall be
    /// provided: a) selector of Entity Types; b) list of Attribute names,
    /// including at least one non-system Attribute; c) NGSI-LD Query,
    /// including at least one non-system Attribute; d) NGSI-LD GeoQuery;
    /// e) local scope. If none of the above is provided, then an error of
    /// type BadRequestData shall be raised (too wide query)." Query Entities
    /// is one operation, so the resource that carries the Query in a body is
    /// bound by it exactly as the one that carries it in the URI.
    #[tokio::test]
    async fn a_too_wide_query_body_is_refused_like_its_uri_twin() {
        let app = app();
        for wide in [
            json!({"type": "Query"}),
            // ids and an id pattern alone are the case the clause names as
            // insufficient ("it is not possible to retrieve a set of entities
            // by only specifying desired Entity identifiers")
            json!({"type": "Query", "entities": [{"id": "urn:ngsi-ld:Vehicle:1"}]}),
            json!({"type": "Query", "entities": [{"idPattern": ".*"}]}),
            // a system Attribute qualifies neither as an attrs list nor as q
            json!({"type": "Query", "attrs": ["createdAt"]}),
            json!({"type": "Query", "q": "createdAt>\"2020-01-01T00:00:00Z\""}),
        ] {
            let resp = post(&app, "/ngsi-ld/v1/entityOperations/query", wide.clone()).await;
            assert_eq!(
                resp.status(),
                StatusCode::BAD_REQUEST,
                "too wide query accepted: {wide}"
            );
            let body = body_json(resp).await;
            assert_eq!(
                body["type"], "https://uri.etsi.org/ngsi-ld/errors/BadRequestData",
                "{body}"
            );
        }

        // Each of the five qualifying inputs on its own is still served.
        for ok in [
            json!({"type": "Query", "entities": [{"type": "Vehicle"}]}),
            json!({"type": "Query", "attrs": ["speed"]}),
            json!({"type": "Query", "q": "speed>100"}),
            json!({"type": "Query", "geoQ": {"georel": "near;maxDistance==2000",
                   "geometry": "Point", "coordinates": "[1,2]"}}),
        ] {
            let resp = post(&app, "/ngsi-ld/v1/entityOperations/query", ok.clone()).await;
            assert_eq!(
                resp.status(),
                StatusCode::OK,
                "qualifying query refused: {ok}"
            );
        }
    }

    /// A Query member lifted out of the body becomes the parameter its GET
    /// twin carries in the URI, where 6.3.4 caps it — the POST form must not
    /// be the cheap way past that cap.
    #[tokio::test]
    async fn query_body_members_are_capped_like_the_uri() {
        let app = app();
        let huge = "a".repeat(crate::bounds::MAX_URI_BYTES + 1);
        let resp = post(
            &app,
            "/ngsi-ld/v1/entityOperations/query",
            json!({"type": "Query", "entities": [{"type": "Vehicle"}], "q": format!("name==\"{huge}\"")}),
        ).await
        ;
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
        let body = body_json(resp).await;
        assert!(
            body["title"].as_str().is_some_and(|t| t.contains("Bad")),
            "{body}"
        );

        // the same member just inside the cap is still served
        let ok = "a".repeat(64);
        let resp = post(
            &app,
            "/ngsi-ld/v1/entityOperations/query",
            json!({"type": "Query", "entities": [{"type": "Vehicle"}], "q": format!("name==\"{ok}\"")}),
        ).await
        ;
        assert_eq!(resp.status(), StatusCode::OK, "a normal query still works");

        // and the array members are assembled under the same cap
        let many: Vec<Value> = (0..600)
            .map(|i| Value::String(format!("attribute-with-a-long-name-{i:04}")))
            .collect();
        let resp = post(
            &app,
            "/ngsi-ld/v1/entityOperations/query",
            json!({"type": "Query", "entities": [{"type": "Vehicle"}], "attrs": many}),
        )
        .await;
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    }

    /// The same cap again, for the members `ordering` and `aggrParams` are
    /// lifted into. 5.2.43 makes `orderBy` a String[] and 5.2.44 makes
    /// `aggrMethods` one, and both are joined into the flat parameter the GET
    /// twin carries in its URI — where 6.3.4's bare 414 caps them. They went
    /// into the map uncapped, so the POST form handed the 4.23 ordering and
    /// the 4.5.19 aggregation parsers a string the GET form cannot express:
    /// one body inside `MAX_BODY_BYTES` holds ~150 000 order keys, each
    /// expanded once per Entity comparison.
    #[tokio::test]
    async fn ordering_and_aggregation_members_are_capped_like_the_uri() {
        let app = app();
        let many: Vec<Value> = (0..600)
            .map(|i| Value::String(format!("attributeWithALongOrderKeyName{i:04}")))
            .collect();
        let resp = post(
            &app,
            "/ngsi-ld/v1/entityOperations/query",
            json!({"type": "Query", "entities": [{"type": "Vehicle"}],
                   "ordering": {"orderBy": many}}),
        )
        .await;
        assert_eq!(
            resp.status(),
            StatusCode::BAD_REQUEST,
            "an over-cap orderBy is refused"
        );

        // the temporal twin carries aggrParams; its aggrMethods is joined the
        // same way and takes the same cap
        let methods: Vec<Value> = (0..600)
            .map(|i| Value::String(format!("totallyUnknownAggregationMethod{i:04}")))
            .collect();
        let resp = post(
            &app,
            "/ngsi-ld/v1/temporal/entityOperations/query",
            json!({"type": "Query", "entities": [{"type": "Vehicle"}],
                   "temporalQ": {"timerel": "before", "timeAt": "2026-01-01T00:00:00Z"},
                   "aggrParams": {"aggrMethods": methods}}),
        )
        .await;
        assert_eq!(
            resp.status(),
            StatusCode::BAD_REQUEST,
            "an over-cap aggrMethods is refused"
        );

        // an ordinary ordering is untouched
        let resp = post(
            &app,
            "/ngsi-ld/v1/entityOperations/query",
            json!({"type": "Query", "entities": [{"type": "Vehicle"}],
                   "ordering": {"orderBy": ["name", "!speed"], "collation": "sk"}}),
        )
        .await;
        assert_eq!(
            resp.status(),
            StatusCode::OK,
            "a normal ordering still works"
        );
    }

    /// The same cap, for the three parameters the `entities` selectors are
    /// lifted into. 5.2.33 makes `entities` an array of EntitySelectors, and
    /// `query_doc_params` folds their `type`, `id` and `idPattern` members
    /// into the flat parameters the GET twin carries in its URI — where
    /// 6.3.4's bare 414 caps them at `MAX_URI_BYTES`. They went into the map
    /// before the cap existed in the function, so the POST form handed the
    /// expansion and store layers a string the GET form cannot express:
    /// ~320 000 selectors fit inside `MAX_BODY_BYTES`, one expanded IRI and
    /// one bind parameter each, past PostgreSQL's 65 535-parameter limit.
    #[tokio::test]
    async fn entity_selector_members_are_capped_like_the_uri() {
        let app = app();
        let many_types: Vec<Value> = (0..600)
            .map(|i| json!({"type": format!("VeryLongTypeNameForTheCap{i:04}")}))
            .collect();
        let resp = post(
            &app,
            "/ngsi-ld/v1/entityOperations/query",
            json!({"type": "Query", "entities": many_types}),
        )
        .await;
        assert_eq!(
            resp.status(),
            StatusCode::BAD_REQUEST,
            "the joined type parameter is capped like its GET twin"
        );

        let many_ids: Vec<Value> = (0..400)
            .map(|i| json!({"id": format!("urn:ngsi-ld:Vehicle:a-fairly-long-identifier-{i:06}")}))
            .collect();
        let resp = post(
            &app,
            "/ngsi-ld/v1/entityOperations/query",
            json!({"type": "Query", "entities": many_ids}),
        )
        .await;
        assert_eq!(
            resp.status(),
            StatusCode::BAD_REQUEST,
            "the joined id parameter is capped like its GET twin"
        );

        let many_pats: Vec<Value> = (0..400)
            .map(|i| json!({"idPattern": format!("^urn:ngsi-ld:Vehicle:pattern-{i:06}.*$")}))
            .collect();
        let resp = post(
            &app,
            "/ngsi-ld/v1/entityOperations/query",
            json!({"type": "Query", "entities": many_pats}),
        )
        .await;
        assert_eq!(
            resp.status(),
            StatusCode::BAD_REQUEST,
            "the joined idPattern parameter is capped like its GET twin"
        );

        // The GeoQuery strings are lifted the same way (5.2.13).
        let resp = post(
            &app,
            "/ngsi-ld/v1/entityOperations/query",
            json!({"type": "Query", "entities": [{"type": "Vehicle"}],
                   "geoQ": {"georel": format!("near;maxDistance=={}", "9".repeat(
                       crate::bounds::MAX_URI_BYTES + 1)),
                            "geometry": "Point", "coordinates": [0, 0]}}),
        )
        .await;
        assert_eq!(
            resp.status(),
            StatusCode::BAD_REQUEST,
            "geoQ georel is capped like its GET twin"
        );

        // …but `coordinates` keeps its own ceiling: MAX_GEO_VERTICES, which a
        // legal polygon can spend more than MAX_URI_BYTES on. Capping it in
        // bytes too would refuse a geometry the broker advertises support for.
        // A circle: a valid ring, as the area relations ask (T-3912).
        let ring: Vec<Value> = (0..=600)
            .map(|i| f64::from(i % 600) * std::f64::consts::TAU / 600.0)
            .map(|a| json!([a.cos() / 100.0, a.sin() / 100.0]))
            .collect();
        let coords = json!([ring]);
        assert!(
            coords.to_string().len() > crate::bounds::MAX_URI_BYTES,
            "the polygon has to exceed the byte cap for this to prove anything"
        );
        let resp = post(
            &app,
            "/ngsi-ld/v1/entityOperations/query",
            json!({"type": "Query", "entities": [{"type": "Vehicle"}],
                   "geoQ": {"georel": "within", "geometry": "Polygon",
                            "coordinates": coords}}),
        )
        .await;
        assert_eq!(
            resp.status(),
            StatusCode::OK,
            "a polygon under MAX_GEO_VERTICES is served whatever it weighs"
        );

        // The cap may not cost an ordinary multi-selector query: 5.2.33's
        // union of a handful of selectors stays well inside it.
        let resp = post(
            &app,
            "/ngsi-ld/v1/entityOperations/query",
            json!({"type": "Query", "entities": [
                {"type": "Vehicle", "id": "urn:ngsi-ld:Vehicle:1"},
                {"type": "Vehicle", "id": "urn:ngsi-ld:Vehicle:2"}
            ]}),
        )
        .await;
        assert_eq!(resp.status(), StatusCode::OK, "a normal union still works");
    }

    async fn get_entity(app: &Router, id: &str) -> Value {
        let resp = app
            .clone()
            .oneshot(
                Request::get(format!("/ngsi-ld/v1/entities/{id}"))
                    .body(Body::empty())
                    .expect("req"),
            )
            .await
            .expect("resp");
        assert_eq!(resp.status(), StatusCode::OK, "entity {id} readable");
        let bytes = resp.into_body().collect().await.expect("body").to_bytes();
        serde_json::from_slice(&bytes).expect("json body")
    }

    /// 6.3.7: `options` is a comma separated list of strings, so the
    /// 6.15.3.1 "update" upsert mode applies whenever it is one member of
    /// the list — existing Entity content is updated, not replaced.
    #[tokio::test]
    async fn upsert_update_mode_in_option_list_merges() {
        let app = app();
        let id = "urn:ngsi-ld:Building:optlist-upsert";
        let resp = post(
            &app,
            "/ngsi-ld/v1/entityOperations/create",
            json!([{"id": id, "type": "Building",
                    "speed": {"type": "Property", "value": 1},
                    "brand": {"type": "Property", "value": "acme"}}]),
        )
        .await;
        assert_eq!(resp.status(), StatusCode::CREATED);
        let resp = post(
            &app,
            "/ngsi-ld/v1/entityOperations/upsert?options=update,sysAttrs",
            json!([{"id": id, "type": "Building",
                    "speed": {"type": "Property", "value": 2}}]),
        )
        .await;
        assert_eq!(resp.status(), StatusCode::NO_CONTENT);
        let doc = get_entity(&app, id).await;
        assert_eq!(doc["speed"]["value"], 2, "the payload attribute is applied");
        // update mode must NOT destroy attributes absent from the payload
        assert_eq!(
            doc["brand"]["value"], "acme",
            "update mode keeps attributes not in the payload: {doc}"
        );
    }

    /// A member that restates what is stored writes nothing (T-5050): its
    /// `modifiedAt` stays where the last real change left it, and the next
    /// real change still lands and moves it.
    #[tokio::test]
    async fn an_unchanged_upsert_member_writes_nothing() {
        let app = app();
        let id = "urn:ngsi-ld:Vehicle:parked";
        let v = |speed: i64| {
            json!([{"id": id, "type": "Vehicle",
                    "speed": {"type": "Property", "value": speed,
                              "observedAt": "2026-10-21T18:00:00Z"}}])
        };
        let uri = "/ngsi-ld/v1/entityOperations/upsert?options=update";
        assert_eq!(post(&app, uri, v(0)).await.status(), StatusCode::CREATED);
        let stamp = |d: &Value| d["modifiedAt"].clone();
        let first = get_entity(&app, &format!("{id}?options=sysAttrs")).await;
        tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        assert_eq!(post(&app, uri, v(0)).await.status(), StatusCode::NO_CONTENT);
        let again = get_entity(&app, &format!("{id}?options=sysAttrs")).await;
        assert_eq!(stamp(&again), stamp(&first), "no change, no write: {again}");
        assert_eq!(again["speed"]["modifiedAt"], first["speed"]["modifiedAt"]);
        tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        assert_eq!(post(&app, uri, v(7)).await.status(), StatusCode::NO_CONTENT);
        let moved = get_entity(&app, &format!("{id}?options=sysAttrs")).await;
        assert_eq!(moved["speed"]["value"], 7);
        assert_ne!(
            stamp(&moved),
            stamp(&first),
            "a real change is written: {moved}"
        );
    }

    /// Only the Entity's and the instances' own stamps are skipped; a
    /// `modifiedAt` inside a value is data.
    #[test]
    fn same_but_stamps_skips_only_the_merge_stamps() {
        let doc = |m: &str, inner: &str| {
            json!({"id": "urn:x", "type": ["T"], "modifiedAt": m,
                   "p": [{"type": "Property", "modifiedAt": m,
                          "value": {"modifiedAt": inner}}]})
        };
        assert!(super::same_but_stamps(&doc("1", "a"), &doc("2", "a")));
        assert!(!super::same_but_stamps(&doc("1", "a"), &doc("1", "b")));
        let mut more = doc("1", "a");
        more["q"] = json!([{"type": "Property", "value": 1}]);
        assert!(!super::same_but_stamps(&doc("1", "a"), &more));
        assert!(!super::same_but_stamps(&more, &doc("1", "a")));
    }

    /// 4.22 + 5.6.9.4 end to end. 5.6.9.4 (PDF p.175): "For each of the
    /// NGSI-LD Entities included in the input Array execute the behaviour
    /// defined by clause 5.6.3, but limited to a local operation… If the
    /// Entity update failed, then a new BatchEntityError shall be added to E
    /// containing the failed Entity ID and the ProblemDetails associated."
    /// 4.22 makes an entity past its `expiresAt` invalid, so 5.6.3 answers
    /// ResourceNotFound and the batch has to report it in E — not in S, and
    /// not by writing to it.
    #[tokio::test]
    async fn batch_update_of_an_expired_entity_is_an_error_not_a_success() {
        let app = app();
        let id = "urn:ngsi-ld:Building:expired-batch-update";
        let resp = post(
            &app,
            "/ngsi-ld/v1/entityOperations/create",
            json!([{"id": id, "type": "Building", "expiresAt": "2020-01-01T00:00:00Z",
                    "speed": {"type": "Property", "value": 1}}]),
        )
        .await;
        assert_eq!(resp.status(), StatusCode::CREATED);

        // The premise the client can see: it is already absent to a read.
        assert_eq!(
            get_status(&app, &format!("/ngsi-ld/v1/entities/{id}")).await,
            StatusCode::NOT_FOUND,
            "4.22: an expired entity does not exist"
        );

        let resp = post(
            &app,
            "/ngsi-ld/v1/entityOperations/update",
            json!([{"id": id, "type": "Building",
                    "speed": {"type": "Property", "value": 2}}]),
        )
        .await;
        assert_eq!(
            resp.status(),
            StatusCode::MULTI_STATUS,
            "5.6.9.5: none updated, so S and E are reported"
        );
        let outcome = body_json(resp).await;
        assert!(
            outcome["success"].as_array().is_none_or(|a| a.is_empty()),
            "an entity every read refuses may not be reported as updated: {outcome}"
        );
        let errors = outcome["errors"].as_array().expect("E array");
        assert_eq!(errors.len(), 1, "{outcome}");
        assert_eq!(errors[0]["entityId"], id, "{outcome}");
        assert!(
            errors[0]["error"]["type"]
                .as_str()
                .is_some_and(|t| t.ends_with("ResourceNotFound")),
            "5.6.3 on an absent entity is ResourceNotFound: {outcome}"
        );

        // and the write did not happen behind the error
        assert_eq!(
            get_status(&app, &format!("/ngsi-ld/v1/entities/{id}")).await,
            StatusCode::NOT_FOUND,
            "still absent"
        );
    }

    /// 6.3.7 + 6.16.3.1: "noOverwrite" as one member of the `options` list
    /// disables Attribute overwrite for Batch Entity Update (5.6.9).
    #[tokio::test]
    async fn batch_update_no_overwrite_in_option_list_is_honoured() {
        let app = app();
        let id = "urn:ngsi-ld:Building:optlist-update";
        let resp = post(
            &app,
            "/ngsi-ld/v1/entityOperations/create",
            json!([{"id": id, "type": "Building",
                    "speed": {"type": "Property", "value": 1}}]),
        )
        .await;
        assert_eq!(resp.status(), StatusCode::CREATED);
        let resp = post(
            &app,
            "/ngsi-ld/v1/entityOperations/update?options=noOverwrite,sysAttrs",
            json!([{"id": id, "type": "Building",
                    "speed": {"type": "Property", "value": 2},
                    "brand": {"type": "Property", "value": "acme"}}]),
        )
        .await;
        // `speed` already existed, so the skip is a partial failure: 5.6.9.5
        // makes 207 with the S and E arrays the only correct answer.
        assert_eq!(
            resp.status(),
            StatusCode::MULTI_STATUS,
            "the skipped instance must be reported"
        );
        let outcome = body_json(resp).await;
        let errs = outcome["errors"].as_array().expect("E array");
        assert_eq!(errs.len(), 1, "{outcome}");
        assert_eq!(errs[0]["entityId"], id, "E names the entity: {outcome}");
        assert!(
            !outcome["success"]
                .as_array()
                .expect("S array")
                .iter()
                .any(|v| v.as_str() == Some(id)),
            "a partially skipped entity must not also be in S: {outcome}"
        );
        let doc = get_entity(&app, id).await;
        assert_eq!(
            doc["speed"]["value"], 1,
            "noOverwrite leaves the existing instance alone: {doc}"
        );
        assert_ne!(doc["speed"]["value"], 2, "the payload must not overwrite");
        assert_eq!(doc["brand"]["value"], "acme", "new attributes are appended");
    }

    /// 5.6.7.4: what merges into the client's S and E arrays is the outcome
    /// of the Entities this broker forwarded. Ids a Context Source invents
    /// are dropped, and its error text is never relayed verbatim.
    #[tokio::test]
    async fn remote_batch_results_are_confined_to_forwarded_ids() {
        let sent = vec!["urn:ngsi-ld:Building:mine".to_owned()];
        let (mut ok, mut err) = (Vec::new(), Vec::new());
        merge_remote_batch(
            207,
            &json!({
                "success": ["urn:ngsi-ld:Building:mine", "urn:ngsi-ld:Secret:peer"],
                "errors": [
                    {"entityId": "urn:ngsi-ld:Building:mine",
                     "error": {"status": 404, "detail": "row 42 of table peer_secrets"}},
                    {"entityId": "urn:ngsi-ld:Secret:other",
                     "error": {"status": 409, "detail": "peer internals"}}
                ]
            }),
            &sent,
            false,
            &mut ok,
            &mut err,
        );
        assert_eq!(
            ok,
            vec![("urn:ngsi-ld:Building:mine".to_owned(), false)],
            "only forwarded ids reach S"
        );
        assert_eq!(err.len(), 1, "only forwarded ids reach E: {err:?}");
        assert_eq!(err[0]["entityId"], "urn:ngsi-ld:Building:mine");
        let dump = Value::Array(err.clone()).to_string();
        assert!(
            !dump.contains("Secret") && !dump.contains("peer"),
            "peer ids and error text must not be relayed: {dump}"
        );
        assert_eq!(err[0]["error"]["status"], 404, "the remote status travels");
        // a 2xx id list is confined the same way
        let (mut ok, mut err) = (Vec::new(), Vec::new());
        merge_remote_batch(
            201,
            &json!(["urn:ngsi-ld:Building:mine", "urn:ngsi-ld:Secret:peer"]),
            &sent,
            true,
            &mut ok,
            &mut err,
        );
        assert_eq!(ok, vec![("urn:ngsi-ld:Building:mine".to_owned(), true)]);
        assert!(err.is_empty());
    }

    /// 5.6.10.3: the Batch Entity Delete input is "an array of Entity IDs"
    /// with no per-operation exemption from this broker's batch ceiling —
    /// an over-cap array is rejected whole and deletes nothing.
    #[tokio::test]
    async fn batch_delete_over_the_item_cap_deletes_nothing() {
        let app = app();
        let id = "urn:ngsi-ld:Building:delete-cap";
        let resp = post(
            &app,
            "/ngsi-ld/v1/entityOperations/create",
            json!([{"id": id, "type": "Building"}]),
        )
        .await;
        assert_eq!(resp.status(), StatusCode::CREATED);
        let mut ids: Vec<Value> = (0..*crate::bounds::MAX_BATCH_ITEMS)
            .map(|i| Value::String(format!("urn:ngsi-ld:Building:cap-{i}")))
            .collect();
        ids.push(Value::String(id.to_owned()));
        let resp = post(
            &app,
            "/ngsi-ld/v1/entityOperations/delete",
            Value::Array(ids),
        )
        .await;
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST, "over-cap delete");
        let doc = body_json(resp).await;
        assert!(
            doc["type"]
                .as_str()
                .expect("problem type")
                .ends_with("BadRequestData"),
            "{doc}"
        );
        // rejected whole: the in-range entity of the same array survives
        get_entity(&app, id).await;
    }

    /// 5.6.8.5 third bullet: when only some Entities succeeded, S is "the
    /// list of Entities successfully created **or updated**" — the
    /// created-only list of the second bullet is the all-succeeded case.
    #[tokio::test]
    async fn upsert_207_success_carries_updated_entities_too() {
        let app = app();
        let existing = "urn:ngsi-ld:Building:upsert-207-old";
        let fresh = "urn:ngsi-ld:Building:upsert-207-new";
        let resp = post(
            &app,
            "/ngsi-ld/v1/entityOperations/create",
            json!([{"id": existing, "type": "Building"}]),
        )
        .await;
        assert_eq!(resp.status(), StatusCode::CREATED);
        let resp = post(
            &app,
            "/ngsi-ld/v1/entityOperations/upsert",
            json!([
                {"id": existing, "type": "Building",
                 "speed": {"type": "Property", "value": 1}},
                {"id": fresh, "type": "Building"},
                "not an entity"
            ]),
        )
        .await;
        assert_eq!(resp.status(), StatusCode::MULTI_STATUS);
        let doc = body_json(resp).await;
        let s: Vec<&str> = doc["success"]
            .as_array()
            .expect("S array")
            .iter()
            .filter_map(Value::as_str)
            .collect();
        assert!(s.contains(&existing), "the updated Entity is in S: {doc}");
        assert!(s.contains(&fresh), "the created Entity is in S: {doc}");
        assert_eq!(doc["errors"].as_array().expect("E array").len(), 1, "{doc}");
    }
}
