// SPDX-License-Identifier: EUPL-1.2
//! `/q/retention/{tenant}`: how long a Tenant keeps each Entity type's
//! current state and history (T-3900, platform ADR-N-046 2.2 and 2.3).
//!
//! One document per Tenant, stored beside the view documents and reloaded
//! with them. A rule names an expanded type IRI and one or two durations:
//! `current` removes an Entity of that type that carries no `expiresAt` of
//! its own once it has not been modified for that long; `history` removes
//! its attribute instances older than that. A type without a rule, and a
//! Tenant without a document, lose nothing. Once a minute every pod sweeps
//! (deleting twice what is already gone deletes nothing), in bounded batches,
//! and counts what it deleted in `antares_retention_deleted_total`.

use super::{bad_tenant, problem, serve, Reloader};
use antares_api::AppState;
use antares_model::TenantId;
use axum::body::Bytes;
use axum::extract::Path;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::Router;
use serde::Deserialize;
use serde_json::Value;
use std::sync::{Arc, Weak};
use std::time::Duration;

/// The id prefix a Tenant's retention document is stored under, beside the
/// view documents; a view id is `[a-z0-9-]`, so none shares a key with it.
pub const RETENTION_ID_PREFIX: &str = "retention:";
/// The most rules one Tenant's document holds.
pub const MAX_RETENTION_RULES: usize = 256;
/// The largest retention document `PUT` accepts, in bytes.
pub const MAX_RETENTION_BYTES: usize = 64 * 1024;
/// How often every pod sweeps.
pub const SWEEP_EVERY: Duration = Duration::from_secs(60);
/// Rows one statement deletes, and statements one rule runs per sweep:
/// a backlog drains over several minutes instead of holding one long lock.
const BATCH: usize = 1000;
const ROUNDS: usize = 50;
/// How long one sweep of one pod may spend deleting: past it the sweep stops
/// and the next minute goes on, so a backlog takes at most a quarter of each
/// minute (T-4084: an 8-hour rule over a 117 GB history held Postgres at its
/// limit and starved every other query of the broker).
// ponytail: the rules run in the same order every sweep, so a backlog of
// the first one delays the next until it drains; rotate the start if a
// second backlog ever waits on a first.
pub const SWEEP_BUDGET: Duration = Duration::from_secs(15);
/// The most rows a preview counts per rule and kind, in one statement.
pub const PREVIEW_CAP: usize = 100_000;
/// The cut of a `history: "off"` rule: every stored instant is before it.
const OFF_CUT: &str = "9999-12-31T23:59:59.999Z";

/// The type a Tenant's default rule names (DM-97): every type without a
/// history rule of its own.
pub const ANY_TYPE: &str = "*";
/// The `type` label of what an Entity's own `historyRetention` deleted, and
/// of what the platform default deleted; a metric labelled by Entity id
/// would grow without bound.
pub const ENTITY_LABEL: &str = "@entity";
/// See [`ENTITY_LABEL`].
pub const PLATFORM_LABEL: &str = "@platform";
/// The most Entities with their own `historyRetention` one sweep reads.
// ponytail: one bounded read per sweep; page it once a deployment holds more.
const MAX_OVERRIDES: usize = 10_000;

/// One rule of a Tenant's document.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
struct RuleDoc {
    #[serde(rename = "type")]
    type_iri: String,
    #[serde(default)]
    current: Option<String>,
    #[serde(default)]
    history: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Document {
    rules: Vec<RuleDoc>,
}

/// One rule as the sweep runs it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Rule {
    /// The expanded Entity type, or [`ANY_TYPE`] for the Tenant's default.
    pub type_iri: String,
    /// How long an Entity not modified lives on, if the rule says.
    pub current: Option<Duration>,
    /// How long an attribute instance is kept, if the rule says.
    pub history: Option<Duration>,
    /// `history: "off"` (T-4275): nothing is recorded for the type, and what
    /// it holds is swept.
    pub history_off: bool,
    /// `history: "forever"` (DM-97): the type keeps all of it, and no level
    /// below (the Tenant's default, the platform's) reaches it.
    pub history_forever: bool,
}

impl Rule {
    /// The rule says something of history, so the levels below leave the
    /// type alone.
    fn rules_history(&self) -> bool {
        self.history.is_some() || self.history_off || self.history_forever
    }
}

/// Every Tenant's rules, as the last reload found them.
pub type Rules = Vec<(TenantId, Vec<Rule>)>;

pub use antares_api::history::{parse_duration, parse_duration_within};

/// The rules of one document, or the first reason it holds none.
///
/// # Errors
/// A document that is not one, a type that is no absolute IRI, a type named
/// twice, a duration out of bounds, a `*` rule with a `current`, or more
/// than [`MAX_RETENTION_RULES`].
pub fn parse(body: &[u8]) -> Result<Vec<Rule>, String> {
    let doc: Document =
        serde_json::from_slice(body).map_err(|e| format!("not a retention document: {e}"))?;
    if doc.rules.len() > MAX_RETENTION_RULES {
        return Err(format!(
            "a document holds at most {MAX_RETENTION_RULES} rules"
        ));
    }
    let mut seen = std::collections::BTreeSet::new();
    doc.rules
        .into_iter()
        .map(|r| {
            let any = r.type_iri == ANY_TYPE;
            // An expanded IRI, as the store holds types: a short name would
            // depend on a @context the document does not carry.
            if !any
                && !r.type_iri.split_once(':').is_some_and(|(scheme, rest)| {
                    !scheme.is_empty()
                        && !rest.is_empty()
                        && !r.type_iri.contains(char::is_whitespace)
                })
            {
                return Err(format!("type {:?} is not an expanded IRI", r.type_iri));
            }
            // A default `current` would remove every Entity of the Tenant
            // nobody wrote for a while, of types no one chose (DM-97).
            if any && r.current.is_some() {
                return Err("the \"*\" rule sets history only, never current".into());
            }
            if !seen.insert(r.type_iri.clone()) {
                return Err(format!("type {:?} has two rules", r.type_iri));
            }
            let word = r
                .history
                .as_deref()
                .filter(|h| ["off", "forever"].contains(h));
            Ok(Rule {
                current: r.current.as_deref().map(parse_duration).transpose()?,
                history: r
                    .history
                    .as_deref()
                    .filter(|_| word.is_none())
                    .map(parse_duration)
                    .transpose()?,
                history_off: word == Some("off"),
                history_forever: word == Some("forever"),
                type_iri: r.type_iri,
            })
        })
        .collect()
}

/// The rules of the stored retention documents among `docs`, by Tenant; a
/// stored document that no longer parses is skipped with a warning, so the
/// sweep deletes nothing for that Tenant rather than guess.
pub(super) fn load<'a>(docs: impl Iterator<Item = (&'a str, &'a Value)>) -> Rules {
    let mut rules = Rules::new();
    for (tenant, doc) in docs {
        let body = doc.get("text").and_then(Value::as_str).unwrap_or_default();
        match TenantId::new(tenant)
            .map_err(|e| e.to_string())
            .and_then(|t| Ok((t, parse(body.as_bytes())?)))
        {
            Ok(rule) => rules.push(rule),
            Err(e) => tracing::warn!(tenant, error = %e, "retention document skipped"),
        }
    }
    rules
}

/// What the write path's history gate 3 needs of `rules` (T-4275, DM-97).
pub(super) fn history_off(rules: &Rules) -> antares_api::HistoryRules {
    let mut out = antares_api::HistoryRules::default();
    for (t, list) in rules {
        for r in list {
            let tenant = t.as_str().to_owned();
            if r.type_iri == ANY_TYPE {
                if r.history_off {
                    out.space_off.insert(tenant);
                }
                continue;
            }
            if r.history_off {
                out.off.insert((tenant.clone(), r.type_iri.clone()));
            }
            if r.rules_history() {
                out.ruled.insert((tenant, r.type_iri.clone()));
            }
        }
    }
    out
}

/// What one sweep deleted of one Tenant's type.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Deleted {
    /// The Tenant.
    pub tenant: String,
    /// The expanded type, [`ANY_TYPE`] for the Tenant's default,
    /// [`ENTITY_LABEL`] or [`PLATFORM_LABEL`].
    pub type_iri: String,
    /// `entity` or `history`.
    pub kind: &'static str,
    /// How many rows.
    pub count: u64,
}

/// One sweep at `now` over the rules the last reload found: every rule's
/// current state, then its history, each in at most [`ROUNDS`] statements
/// of [`BATCH`] rows. Logs, counts and totals per pod what it deleted, never
/// what it held.
pub(super) async fn sweep(r: &Reloader, now: chrono::DateTime<chrono::Utc>) -> Vec<Deleted> {
    sweep_within(r, now, SWEEP_BUDGET).await
}

/// [`sweep`] with `budget` in place of [`SWEEP_BUDGET`].
pub(super) async fn sweep_within(
    r: &Reloader,
    now: chrono::DateTime<chrono::Utc>,
    budget: Duration,
) -> Vec<Deleted> {
    let plan = Plan {
        rules: &r.retention_rules(),
        platform: r.platform_history,
        now,
        dry_run: false,
        entities: true,
    };
    let out = retain(&r.state, &plan, budget).await;
    let mut totals = r
        .deleted
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    for d in &out {
        let (tenant, ty, kind, count) = (d.tenant.as_str(), d.type_iri.as_str(), d.kind, d.count);
        metrics::counter!("antares_retention_deleted_total",
            "tenant" => tenant.to_owned(), "type" => ty.to_owned(), "kind" => kind)
        .increment(count);
        tracing::info!(tenant, r#type = ty, kind, count, "retention deleted");
        let total = totals
            .entry((tenant.to_owned(), ty.to_owned(), kind))
            .or_default();
        *total = total.saturating_add(count);
    }
    out
}

/// What one run of [`retain`] covers.
struct Plan<'a> {
    rules: &'a Rules,
    /// The platform default (`ANTARES_RETENTION_HISTORY_DEFAULT`), applied to
    /// every Tenant without a `*` rule; `None` keeps what nothing rules.
    platform: Option<Duration>,
    now: chrono::DateTime<chrono::Utc>,
    dry_run: bool,
    /// Run level 1, the Entities' own retention; a preview of one draft
    /// document leaves it out, as the draft does not change it.
    entities: bool,
}

/// One statement kind the sweep repeats, owning what it names.
enum Job {
    /// Current state of one type (DM-83).
    Current(String),
    /// History of one Entity, by its own `historyRetention`.
    Entity(String),
    /// History of one type, but not the Entities that rule themselves.
    Type {
        type_iri: String,
        except_ids: Vec<String>,
    },
    /// History of every type without a history rule of its own.
    Rest {
        except_types: Vec<String>,
        except_ids: Vec<String>,
    },
}

impl Job {
    /// The history the job reaches, or the type whose current state it does.
    fn scope(&self) -> Result<antares_api::HistoryScope<'_>, &str> {
        match self {
            Job::Current(type_iri) => Err(type_iri),
            Job::Entity(id) => Ok(antares_api::HistoryScope::Entity(id)),
            Job::Type {
                type_iri,
                except_ids,
            } => Ok(antares_api::HistoryScope::Type {
                type_iri,
                except_ids,
            }),
            Job::Rest {
                except_types,
                except_ids,
            } => Ok(antares_api::HistoryScope::Rest {
                except_types,
                except_ids,
            }),
        }
    }
}

/// One job of one Tenant before one cut, recorded under `label`.
struct Step {
    tenant: TenantId,
    label: String,
    job: Job,
    before: String,
}

/// The Entities with a valid `historyRetention` of their own, by Tenant,
/// with the cut each keeps; one that is not a duration nor `off` is logged
/// and left to its type's rule.
async fn overrides(
    state: &AppState,
    now: chrono::DateTime<chrono::Utc>,
) -> std::collections::BTreeMap<String, Vec<(String, String)>> {
    let mut out: std::collections::BTreeMap<String, Vec<(String, String)>> =
        std::collections::BTreeMap::new();
    let found = match state.store.history_overrides(MAX_OVERRIDES).await {
        Ok(found) => found,
        Err(e) => {
            tracing::warn!(error = %e, "retention: entities with their own historyRetention not read");
            return out;
        }
    };
    for (tenant, id, own) in found {
        let before = match antares_api::history::retention_of(&own) {
            Ok(antares_api::history::OwnRetention::Off) => OFF_CUT.to_owned(),
            Ok(antares_api::history::OwnRetention::Keep(keep)) => cut(now, keep),
            Err(why) => {
                tracing::warn!(
                    tenant,
                    entity = id.as_str(),
                    reason = why.as_str(),
                    "historyRetention ignored; the type's rule applies"
                );
                continue;
            }
        };
        out.entry(tenant).or_default().push((id, before));
    }
    out
}

/// The instant `keep` before `now`, as the store compares it.
fn cut(now: chrono::DateTime<chrono::Utc>, keep: Duration) -> String {
    // A rule holds at most ten years, so the cut is always in range.
    chrono::Duration::from_std(keep)
        .ok()
        .and_then(|k| now.checked_sub_signed(k))
        .unwrap_or(chrono::DateTime::<chrono::Utc>::MIN_UTC)
        .to_rfc3339_opts(chrono::SecondsFormat::Millis, true)
}

/// The steps of `plan`, level by level (DM-97): the Entities' own retention,
/// then each type's rule, then the Tenant's `*` rule or the platform default
/// for the rest of every Tenant.
async fn steps(state: &AppState, plan: &Plan<'_>) -> Vec<Step> {
    let own = overrides(state, plan.now).await;
    let own_ids = |t: &str| -> Vec<String> {
        own.get(t)
            .into_iter()
            .flatten()
            .map(|(id, _)| id.clone())
            .collect()
    };
    let step = |tenant: &TenantId, label: &str, job, before| Step {
        tenant: tenant.clone(),
        label: label.to_owned(),
        job,
        before,
    };
    let mut out = Vec::new();
    if plan.entities {
        // A name the store hands back is a Tenant's; one that is not has nothing to sweep.
        let tenants = own
            .iter()
            .filter_map(|(t, list)| Some((TenantId::new(t).ok()?, list)));
        for (t, list) in tenants {
            for (id, before) in list {
                out.push(step(
                    &t,
                    ENTITY_LABEL,
                    Job::Entity(id.clone()),
                    before.clone(),
                ));
            }
        }
    }
    let mut ruled = std::collections::BTreeSet::new();
    for (t, list) in plan.rules {
        ruled.insert(t.as_str().to_owned());
        let except_ids = own_ids(t.as_str());
        for rule in list.iter().filter(|r| r.type_iri != ANY_TYPE) {
            if let Some(keep) = rule.current {
                let job = Job::Current(rule.type_iri.clone());
                out.push(step(t, &rule.type_iri, job, cut(plan.now, keep)));
            }
            let before = match (rule.history_off, rule.history) {
                (true, _) => OFF_CUT.to_owned(),
                (false, Some(keep)) => cut(plan.now, keep),
                (false, None) => continue,
            };
            let job = Job::Type {
                type_iri: rule.type_iri.clone(),
                except_ids: except_ids.clone(),
            };
            out.push(step(t, &rule.type_iri, job, before));
        }
        out.extend(rest(plan, t, list, except_ids));
    }
    // The platform default reaches every other Tenant too.
    // ponytail: every Tenant in name order each sweep, the budget the bound;
    // rotate the start once a deployment's Tenants outgrow one sweep.
    if plan.platform.is_some() && !plan.dry_run {
        let tenants = state.store.tenant_ids().await.unwrap_or_else(|e| {
            tracing::warn!(error = %e, "retention: tenants not listed; the platform default waits");
            Vec::new()
        });
        let others = tenants
            .iter()
            .filter(|t| !ruled.contains(*t))
            .filter_map(|t| TenantId::new(t).ok());
        for t in others {
            let except_ids = own_ids(t.as_str());
            out.extend(rest(plan, &t, &[], except_ids));
        }
    }
    out
}

/// The step of the Tenant's `*` rule, or of the platform default, over every
/// type of `t` without a history rule of its own; none when it keeps all.
fn rest(plan: &Plan<'_>, t: &TenantId, list: &[Rule], except_ids: Vec<String>) -> Option<Step> {
    let (label, before) = match list.iter().find(|r| r.type_iri == ANY_TYPE) {
        Some(r) if r.history_off => (ANY_TYPE, OFF_CUT.to_owned()),
        Some(r) => (ANY_TYPE, cut(plan.now, r.history?)),
        None => (PLATFORM_LABEL, cut(plan.now, plan.platform?)),
    };
    let except_types = list
        .iter()
        .filter(|r| r.type_iri != ANY_TYPE && r.rules_history())
        .map(|r| r.type_iri.clone())
        .collect();
    Some(Step {
        tenant: t.clone(),
        label: label.to_owned(),
        job: Job::Rest {
            except_types,
            except_ids,
        },
        before,
    })
}

/// What `plan` deletes, or with `dry_run` would delete, step by step: a dry
/// run counts at most [`PREVIEW_CAP`] rows per step in one statement; a
/// delete starts no statement once `budget` has passed since it began.
async fn retain(state: &AppState, plan: &Plan<'_>, budget: Duration) -> Vec<Deleted> {
    let mut run = Runner {
        state,
        started: std::time::Instant::now(),
        budget,
        dry_run: plan.dry_run,
        out: Vec::new(),
    };
    for step in steps(state, plan).await {
        if !run.drain(&step).await {
            break;
        }
    }
    run.out
}

/// The rounds of one sweep, against its budget.
struct Runner<'a> {
    state: &'a AppState,
    started: std::time::Instant,
    budget: Duration,
    dry_run: bool,
    out: Vec<Deleted>,
}

impl Runner<'_> {
    /// Run `step` in rounds until a short batch, and record what it deleted
    /// under its label; `false` once the budget is spent.
    async fn drain(&mut self, step: &Step) -> bool {
        let (batch, rounds) = if self.dry_run {
            (PREVIEW_CAP, 1)
        } else {
            (BATCH, ROUNDS)
        };
        let (t, label, before) = (&step.tenant, step.label.as_str(), step.before.as_str());
        let tenant = t.as_str();
        let scope = step.job.scope();
        let kind = if scope.is_ok() { "history" } else { "entity" };
        let mut count = 0u64;
        let mut spent = false;
        for _ in 0..rounds {
            if !self.dry_run && self.started.elapsed() >= self.budget {
                spent = true;
                break;
            }
            let n = match scope {
                Ok(scope) => {
                    self.state
                        .temporal
                        .retain_history(t, scope, before, batch, self.dry_run)
                        .await
                }
                Err(type_iri) => {
                    self.state
                        .store
                        .retain_current(t, type_iri, before, batch, self.dry_run)
                        .await
                }
            };
            match n {
                Ok(n) => {
                    count = count.saturating_add(n);
                    if n < batch as u64 {
                        break;
                    }
                }
                Err(e) => {
                    tracing::warn!(tenant, r#type = label, kind, error = %e, "retention sweep failed");
                    break;
                }
            }
        }
        if count > 0 {
            self.out.push(Deleted {
                tenant: tenant.to_owned(),
                type_iri: label.to_owned(),
                kind,
                count,
            });
        }
        if spent {
            tracing::info!(
                budget_ms = u64::try_from(self.budget.as_millis()).unwrap_or(u64::MAX),
                "retention sweep budget spent; the next sweep goes on"
            );
        }
        !spent
    }
}

/// `/q/retention/{tenant}`: GET the document as written, PUT one, DELETE
/// it; `POST …/preview` counts what a document would delete now, `GET
/// …/deleted` totals what this pod's sweeps deleted since it started.
pub(super) fn routes(weak: &Weak<Reloader>) -> Router {
    let (read, write, remove) = (weak.clone(), weak.clone(), weak.clone());
    let (count, totals) = (weak.clone(), weak.clone());
    Router::new()
        .route(
            "/retention/{tenant}/preview",
            post(move |Path(tenant): Path<String>, body: Bytes| {
                serve(count.clone(), |r| preview(r, tenant, body))
            }),
        )
        .route(
            "/retention/{tenant}/deleted",
            get(move |Path(tenant): Path<String>| {
                serve(totals.clone(), |r| async move { Ok(deleted(&r, &tenant)) })
            }),
        )
        .route(
            "/retention/{tenant}",
            get(move |Path(t): Path<String>| serve(read.clone(), |r| get_doc(r, t)))
                .put(move |Path(t): Path<String>, body: Bytes| {
                    serve(write.clone(), |r| put_doc(r, t, body))
                })
                .delete(move |Path(t): Path<String>| serve(remove.clone(), |r| delete_doc(r, t))),
        )
}

fn key(tenant: &str) -> Option<String> {
    TenantId::new(tenant)
        .ok()
        .map(|t| format!("{RETENTION_ID_PREFIX}{}", t.as_str()))
}

fn none() -> Response {
    problem(
        StatusCode::NOT_FOUND,
        "ResourceNotFound",
        "no retention document for this tenant",
    )
}

async fn get_doc(r: Arc<Reloader>, tenant: String) -> Result<Response, String> {
    super::stored_json(&r, key(&tenant), none).await
}

async fn put_doc(r: Arc<Reloader>, tenant: String, body: Bytes) -> Result<Response, String> {
    let Some(key) = key(&tenant) else {
        return Ok(bad_tenant());
    };
    if let Some(refusal) = refused(&body) {
        return Ok(refusal);
    }
    let text = String::from_utf8_lossy(&body).into_owned();
    let replaced = r
        .state
        .put_view_doc(&key, serde_json::json!({"id": key, "text": text}))
        .await?;
    r.reload().await?;
    Ok(if replaced {
        StatusCode::NO_CONTENT
    } else {
        StatusCode::CREATED
    }
    .into_response())
}

/// The document `body` would be refused for, as `put_doc` answers it.
fn refused(body: &[u8]) -> Option<Response> {
    if body.len() > MAX_RETENTION_BYTES {
        return Some(problem(
            StatusCode::PAYLOAD_TOO_LARGE,
            "BadRequestData",
            &format!("a retention document is at most {MAX_RETENTION_BYTES} bytes"),
        ));
    }
    parse(body)
        .err()
        .map(|why| problem(StatusCode::BAD_REQUEST, "BadRequestData", &why))
}

fn counts(rows: impl Iterator<Item = (String, &'static str, u64)>) -> Response {
    let rows: Vec<Value> = rows
        .map(|(ty, kind, count)| serde_json::json!({"type": ty, "kind": kind, "count": count}))
        .collect();
    axum::Json(serde_json::json!({ "counts": rows })).into_response()
}

async fn preview(r: Arc<Reloader>, tenant: String, body: Bytes) -> Result<Response, String> {
    let Ok(t) = TenantId::new(&tenant) else {
        return Ok(bad_tenant());
    };
    if let Some(refusal) = refused(&body) {
        return Ok(refusal);
    }
    let rules = vec![(t, parse(&body)?)];
    let plan = Plan {
        rules: &rules,
        platform: r.platform_history,
        now: chrono::Utc::now(),
        dry_run: true,
        entities: false,
    };
    let found = retain(&r.state, &plan, SWEEP_BUDGET).await;
    Ok(counts(
        found.into_iter().map(|d| (d.type_iri, d.kind, d.count)),
    ))
}

fn deleted(r: &Reloader, tenant: &str) -> Response {
    let Ok(t) = TenantId::new(tenant) else {
        return bad_tenant();
    };
    let totals = r
        .deleted
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .clone();
    counts(
        totals
            .into_iter()
            .filter(|((owner, _, _), _)| owner == t.as_str())
            .map(|((_, ty, kind), count)| (ty, kind, count)),
    )
}

async fn delete_doc(r: Arc<Reloader>, tenant: String) -> Result<Response, String> {
    let Some(key) = key(&tenant) else {
        return Ok(bad_tenant());
    };
    if !r.state.delete_view_doc(&key).await? {
        return Ok(none());
    }
    r.reload().await?;
    Ok(StatusCode::NO_CONTENT.into_response())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn durations_read_iso_and_short_forms_within_bounds() {
        let secs = |s: &str| parse_duration(s).map(|d| d.as_secs());
        assert_eq!(secs("PT5M"), Ok(300));
        assert_eq!(secs("PT8H"), Ok(28_800));
        assert_eq!(secs("P1DT12H"), Ok(129_600));
        assert_eq!(secs("P30D"), Ok(2_592_000));
        assert_eq!(secs("5m"), Ok(300));
        assert_eq!(secs("8h"), Ok(28_800));
        assert_eq!(secs("90s"), Ok(90));
        assert_eq!(secs("14d"), Ok(1_209_600));
        for bad in [
            "", "P", "PT", "PT30S", "59s", "0m", "-5m", "5w", "P3651D", "PT5X", "P1H", "5", "m",
        ] {
            assert!(parse_duration(bad).is_err(), "{bad:?} parsed");
        }
    }

    #[test]
    fn a_document_names_expanded_types_once() {
        let ok =
            br#"{"rules":[{"type":"https://example.org/Vehicle","current":"PT5M","history":"PT8H"},
                              {"type":"https://example.org/Stop"}]}"#;
        let rules = parse(ok).unwrap_or_default();
        assert_eq!(rules.len(), 2);
        assert_eq!(
            rules.first().and_then(|r| r.current),
            Some(Duration::from_secs(300))
        );
        assert_eq!(
            rules.get(1).map(|r| (r.current, r.history)),
            Some((None, None))
        );
        // T-4275: history "off" is no duration; any other word still is one or nothing.
        let off =
            parse(br#"{"rules":[{"type":"https://e.org/V","history":"off"}]}"#).unwrap_or_default();
        assert_eq!(
            off.first().map(|r| (r.history, r.history_off)),
            Some((None, true))
        );
        let pair = ("city".to_owned(), "https://e.org/V".to_owned());
        assert_eq!(
            history_off(&vec![(TenantId::new("city").expect("tenant"), off)]),
            antares_api::HistoryRules {
                off: std::collections::HashSet::from([pair.clone()]),
                ruled: std::collections::HashSet::from([pair]),
                space_off: std::collections::HashSet::new(),
            }
        );
        assert!(rules.iter().all(|r| !r.history_off));
        for bad in [
            &br#"{"rules":[{"type":"https://e.org/V","history":"Off"}]}"#[..],
            br#"{"rules":[{"type":"https://e.org/V","history":"none"}]}"#,
            br#"{"rules":[{"type":"Vehicle","current":"PT5M"}]}"#,
            br#"{"rules":[{"type":"https://e.org/V"},{"type":"https://e.org/V"}]}"#,
            br#"{"rules":[{"type":"https://e.org/V","current":"PT0M"}]}"#,
            br#"{"rules":[{"type":"https://e.org/V","ttl":"PT5M"}]}"#,
            br#"{"rules":[{"type":":Vehicle"}]}"#,
            br#"{"rules":[{"type":"https:"}]}"#,
            br#"{"rules":[{"type":"https://e.org/A Vehicle"}]}"#,
            br#"{"rules":{}}"#,
            b"not json",
        ] {
            assert!(parse(bad).is_err(), "{}", String::from_utf8_lossy(bad));
        }
    }

    /// DM-97: the `*` rule is the Tenant's default, history only; `forever`
    /// is a word like `off`, and a type with a history rule of any kind is
    /// out of the default's reach while a `current`-only rule is not.
    #[test]
    fn the_star_rule_is_history_only_and_forever_rules_a_type() {
        let doc = br#"{"rules":[{"type":"*","history":"off"},
                                 {"type":"https://e.org/Keep","history":"forever"},
                                 {"type":"https://e.org/Now","current":"PT5M"},
                                 {"type":"https://e.org/Week","history":"P7D"}]}"#;
        let rules = parse(doc).unwrap_or_default();
        assert_eq!(rules.len(), 4);
        assert_eq!(
            rules
                .get(1)
                .map(|r| (r.history, r.history_off, r.history_forever)),
            Some((None, false, true))
        );
        let gate = history_off(&vec![(TenantId::new("city").expect("tenant"), rules)]);
        let pair = |t: &str| ("city".to_owned(), format!("https://e.org/{t}"));
        assert_eq!(
            gate.space_off,
            std::collections::HashSet::from(["city".to_owned()])
        );
        assert!(gate.off.is_empty());
        assert_eq!(
            gate.ruled,
            std::collections::HashSet::from([pair("Keep"), pair("Week")])
        );
        for bad in [
            &br#"{"rules":[{"type":"*","current":"PT5M"}]}"#[..],
            br#"{"rules":[{"type":"*","history":"P7D"},{"type":"*"}]}"#,
            br#"{"rules":[{"type":"**","history":"P7D"}]}"#,
            br#"{"rules":[{"type":"*","history":"always"}]}"#,
        ] {
            assert!(parse(bad).is_err(), "{}", String::from_utf8_lossy(bad));
        }
    }

    #[test]
    fn a_document_holds_at_most_the_rule_limit() {
        let doc = |n: usize| {
            let rules: Vec<_> = (0..n)
                .map(|i| serde_json::json!({"type": format!("https://e.org/T{i}")}))
                .collect();
            serde_json::json!({ "rules": rules }).to_string()
        };
        assert!(parse(doc(MAX_RETENTION_RULES).as_bytes()).is_ok());
        assert!(parse(doc(MAX_RETENTION_RULES.saturating_add(1)).as_bytes()).is_err());
    }

    #[test]
    fn load_skips_a_document_that_no_longer_parses_and_keeps_the_rest() {
        let good =
            serde_json::json!({"text": r#"{"rules":[{"type":"https://e.org/V","history":"5m"}]}"#});
        let bad = serde_json::json!({"text": "nope"});
        let rules = load([("city", &good), ("port", &bad), ("Not One", &good)].into_iter());
        assert_eq!(
            rules
                .iter()
                .map(|(t, r)| (t.as_str(), r.len()))
                .collect::<Vec<_>>(),
            [("city", 1)]
        );
    }
}
