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
/// The bounds a duration has to sit in: one minute to ten years.
const SHORTEST: Duration = Duration::from_secs(60);
const LONGEST: Duration = Duration::from_hours(87_600);

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
    /// The expanded Entity type.
    pub type_iri: String,
    /// How long an Entity not modified lives on, if the rule says.
    pub current: Option<Duration>,
    /// How long an attribute instance is kept, if the rule says.
    pub history: Option<Duration>,
}

/// Every Tenant's rules, as the last reload found them.
pub type Rules = Vec<(TenantId, Vec<Rule>)>;

/// A duration as ISO 8601 (`PT5M`, `PT8H`, `P30D`, `P1DT12H`) or in short
/// form (`90s`, `5m`, `8h`, `14d`), between one minute and ten years.
///
/// # Errors
/// Why `raw` is not one.
pub fn parse_duration(raw: &str) -> Result<Duration, String> {
    parse_duration_within(raw, SHORTEST, LONGEST)
}

/// A duration as [`parse_duration`] reads it, between `shortest` and
/// `longest` (the quality sweep's `maxAge` starts at one second, T-4018).
///
/// # Errors
/// Why `raw` is not one.
pub fn parse_duration_within(
    raw: &str,
    shortest: Duration,
    longest: Duration,
) -> Result<Duration, String> {
    let bad = || format!("{raw:?} is not a duration such as PT5M, PT8H, P30D, 5m, 8h or 14d");
    let secs = if let Some(iso) = raw.strip_prefix('P') {
        let (days, time) = match iso.split_once('T') {
            Some((d, t)) if !t.is_empty() => (d, Some(t)),
            Some(_) => return Err(bad()),
            None => (iso, None),
        };
        let mut total = 0u64;
        if !days.is_empty() {
            total = days
                .strip_suffix('D')
                .and_then(|n| n.parse::<u64>().ok())
                .ok_or_else(bad)?
                .checked_mul(86_400)
                .ok_or_else(bad)?;
        } else if time.is_none() {
            return Err(bad());
        }
        let mut rest = time.unwrap_or_default();
        for (unit, scale) in [('H', 3600u64), ('M', 60), ('S', 1)] {
            if let Some((n, tail)) = rest.split_once(unit) {
                let n = n.parse::<u64>().map_err(|_| bad())?;
                total = n
                    .checked_mul(scale)
                    .and_then(|n| total.checked_add(n))
                    .ok_or_else(bad)?;
                rest = tail;
            }
        }
        if !rest.is_empty() {
            return Err(bad());
        }
        total
    } else {
        let split = raw.len().checked_sub(1).ok_or_else(bad)?;
        let (n, unit) = (
            raw.get(..split).ok_or_else(bad)?,
            raw.get(split..).ok_or_else(bad)?,
        );
        let scale = match unit {
            "s" => 1u64,
            "m" => 60,
            "h" => 3600,
            "d" => 86_400,
            _ => return Err(bad()),
        };
        n.parse::<u64>()
            .ok()
            .and_then(|n| n.checked_mul(scale))
            .ok_or_else(bad)?
    };
    let d = Duration::from_secs(secs);
    if d < shortest || d > longest {
        return Err(format!(
            "{raw:?} is outside {}s to {}s",
            shortest.as_secs(),
            longest.as_secs()
        ));
    }
    Ok(d)
}

/// The rules of one document, or the first reason it holds none.
///
/// # Errors
/// A document that is not one, a type that is no absolute IRI, a type named
/// twice, a duration out of bounds, or more than [`MAX_RETENTION_RULES`].
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
            // An expanded IRI, as the store holds types: a short name would
            // depend on a @context the document does not carry.
            if !r.type_iri.split_once(':').is_some_and(|(scheme, rest)| {
                !scheme.is_empty() && !rest.is_empty() && !r.type_iri.contains(char::is_whitespace)
            }) {
                return Err(format!("type {:?} is not an expanded IRI", r.type_iri));
            }
            if !seen.insert(r.type_iri.clone()) {
                return Err(format!("type {:?} has two rules", r.type_iri));
            }
            Ok(Rule {
                current: r.current.as_deref().map(parse_duration).transpose()?,
                history: r.history.as_deref().map(parse_duration).transpose()?,
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

/// What one sweep deleted of one Tenant's type.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Deleted {
    /// The Tenant.
    pub tenant: String,
    /// The expanded type.
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
    let out = retain(&r.state, &r.retention_rules(), now, false, budget).await;
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

/// What `rules` delete at `now`, or with `dry_run` would delete: a dry run
/// counts at most [`PREVIEW_CAP`] rows per rule and kind in one statement.
/// A delete starts no statement once `budget` has passed since it began.
async fn retain(
    state: &AppState,
    rules: &Rules,
    now: chrono::DateTime<chrono::Utc>,
    dry_run: bool,
    budget: Duration,
) -> Vec<Deleted> {
    let started = std::time::Instant::now();
    let mut spent = false;
    let (batch, rounds) = if dry_run {
        (PREVIEW_CAP, 1)
    } else {
        (BATCH, ROUNDS)
    };
    let mut out = Vec::new();
    for (t, list) in rules {
        let tenant = t.as_str();
        for rule in list {
            for (kind, keep) in [("entity", rule.current), ("history", rule.history)] {
                let Some(keep) = keep else {
                    continue;
                };
                // A rule holds at most ten years, so the cut is always in range.
                let before = chrono::Duration::from_std(keep)
                    .ok()
                    .and_then(|k| now.checked_sub_signed(k))
                    .unwrap_or(chrono::DateTime::<chrono::Utc>::MIN_UTC)
                    .to_rfc3339_opts(chrono::SecondsFormat::Millis, true);
                let mut count = 0u64;
                for _ in 0..rounds {
                    if !dry_run && started.elapsed() >= budget {
                        spent = true;
                        break;
                    }
                    let n = if kind == "entity" {
                        state
                            .store
                            .retain_current(t, &rule.type_iri, &before, batch, dry_run)
                            .await
                    } else {
                        state
                            .temporal
                            .retain_history(t, &rule.type_iri, &before, batch, dry_run)
                            .await
                    };
                    match n {
                        Ok(n) => {
                            count = count.saturating_add(n);
                            if n < batch as u64 {
                                break;
                            }
                        }
                        Err(e) => {
                            tracing::warn!(tenant, r#type = rule.type_iri.as_str(), kind, error = %e, "retention sweep failed");
                            break;
                        }
                    }
                }
                if count > 0 {
                    out.push(Deleted {
                        tenant: tenant.to_owned(),
                        type_iri: rule.type_iri.clone(),
                        kind,
                        count,
                    });
                }
                if spent {
                    tracing::info!(
                        budget_ms = u64::try_from(budget.as_millis()).unwrap_or(u64::MAX),
                        "retention sweep budget spent; the next sweep goes on"
                    );
                    return out;
                }
            }
        }
    }
    out
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
    let found = retain(&r.state, &rules, chrono::Utc::now(), true, SWEEP_BUDGET).await;
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
        for bad in [
            &br#"{"rules":[{"type":"Vehicle","current":"PT5M"}]}"#[..],
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
