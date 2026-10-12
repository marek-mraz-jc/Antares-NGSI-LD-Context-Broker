// SPDX-License-Identifier: EUPL-1.2
//! `/q/quality/{tenant}`: the `maxAge` freshness rules of a Tenant's space and
//! the sweep that quarantines what they find stale (T-4018, platform
//! ADR-N-046 2.5 and 2.6, DM-90, DM-91).
//!
//! One document per Tenant, stored beside the view and retention documents
//! and reloaded with them. An entry names a rule, an expanded type and
//! attribute IRI and a `maxAge`; the entries of one name are one rule (a
//! space's rule reaches the broker once per expansion of its type, and an
//! Entity carries one mark per name), judged together. Once a minute every
//! pod sweeps: an Entity of one of the rule's types whose attribute for that
//! type has an instance observed longer ago than `maxAge` gets
//! `quarantinedAt` and the rule's name in `quarantineRules`; the name goes
//! again when no instance is stale (or the attribute is gone), and both
//! Properties go with the last name. Marking twice what is already marked
//! changes nothing, so pods racing each other agree.
//!
//! The mark narrows, never widens: the Portal's permits hide an Entity that
//! carries `quarantinedAt` from every role but the space's owner and steward.
//! It is maintenance metadata, not a client write: the Entity's `modifiedAt`
//! stays as it was (so the mark never holds off retention), and it leaves no
//! history and sends no notification.

use super::{bad_tenant, problem, serve, Reloader};
use antares_api::AppState;
use antares_model::TenantId;
use axum::body::Bytes;
use axum::extract::Path;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::Router;
use serde::Deserialize;
use serde_json::{json, Value};
use std::sync::{Arc, Weak};
use std::time::Duration;

/// The id prefix a Tenant's quality document is stored under, beside the
/// view documents; a view id is `[a-z0-9-]`, so none shares a key with it.
pub const QUALITY_ID_PREFIX: &str = "quality:";
/// The most rules one Tenant's document holds (a space holds at most 64,
/// DM-92; a Tenant may hold several spaces' worth through a hub).
pub const MAX_QUALITY_RULES: usize = 256;
/// The largest quality document `PUT` accepts, in bytes.
pub const MAX_QUALITY_BYTES: usize = 64 * 1024;
/// The Property that marks a quarantined Entity (DM-90), under the NGSI-LD
/// default context: the expansion of `quarantinedAt` in a payload whose
/// @context does not define it, which is how the gateway's write path marks.
pub const QUARANTINED_AT: &str = "https://uri.etsi.org/ngsi-ld/default-context/quarantinedAt";
/// The Property naming the rules that quarantined an Entity (DM-90).
pub const QUARANTINE_RULES: &str = "https://uri.etsi.org/ngsi-ld/default-context/quarantineRules";
/// Entities one scan reads per page, and pages one rule reads per sweep:
/// a type larger than that is judged over several minutes, never in one
/// long read.
const PAGE: usize = 1000;
const ROUNDS: usize = 50;
/// The bounds of a `maxAge`: one second (the platform's shortest) to ten
/// years.
const SHORTEST: Duration = Duration::from_secs(1);
const LONGEST: Duration = Duration::from_hours(87_600);

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RuleDoc {
    name: String,
    #[serde(rename = "type")]
    type_iri: String,
    attribute: String,
    #[serde(rename = "maxAge")]
    max_age: String,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Document {
    rules: Vec<RuleDoc>,
}

/// One rule as the sweep runs it: every entry of one name.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Rule {
    /// The name the Portal lists the rule by, written into `quarantineRules`.
    pub name: String,
    /// The expanded Entity types and, for each, the expanded attribute whose
    /// `observedAt` is judged, in the order the document lists them.
    pub checks: Vec<(String, String)>,
    /// How old an observation may be.
    pub max_age: Duration,
}

/// Every Tenant's rules, as the last reload found them.
pub type Rules = Vec<(TenantId, Vec<Rule>)>;

/// Whether `raw` is an absolute IRI as the store holds types and attributes:
/// a short name would depend on a @context the document does not carry.
fn expanded(raw: &str) -> bool {
    raw.split_once(':')
        .is_some_and(|(scheme, rest)| !scheme.is_empty() && !rest.is_empty())
        && !raw.contains(char::is_whitespace)
}

/// A rule name as the platform writes it: a DNS-1123 label.
fn label(raw: &str) -> bool {
    (1..=63).contains(&raw.len())
        && raw
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
        && !raw.starts_with('-')
        && !raw.ends_with('-')
}

/// The rules of one document, its entries grouped by name, or the first
/// reason it holds none.
///
/// # Errors
/// A document that is not one, a name that is no DNS-1123 label, one name
/// with two `maxAge` values or the same type and attribute twice, a type or attribute that is no absolute IRI, a `maxAge` that is
/// no duration of one second to ten years, or more than
/// [`MAX_QUALITY_RULES`] rules.
pub fn parse(body: &[u8]) -> Result<Vec<Rule>, String> {
    let doc: Document =
        serde_json::from_slice(body).map_err(|e| format!("not a quality document: {e}"))?;
    if doc.rules.len() > MAX_QUALITY_RULES {
        return Err(format!(
            "a document holds at most {MAX_QUALITY_RULES} rules"
        ));
    }
    let mut rules: Vec<Rule> = Vec::new();
    for r in doc.rules {
        if !label(&r.name) {
            return Err(format!("name {:?} is not a DNS-1123 label", r.name));
        }
        for (field, iri) in [("type", &r.type_iri), ("attribute", &r.attribute)] {
            if !expanded(iri) {
                return Err(format!("{field} {iri:?} is not an expanded IRI"));
            }
        }
        let max_age = super::retention::parse_duration_within(&r.max_age, SHORTEST, LONGEST)?;
        let check = (r.type_iri, r.attribute);
        match rules.iter_mut().find(|known| known.name == r.name) {
            Some(known) if known.max_age != max_age => {
                return Err(format!("name {:?} has two maxAge values", r.name));
            }
            Some(known) if known.checks.contains(&check) => {
                return Err(format!(
                    "name {:?} lists type {:?} and attribute {:?} twice",
                    r.name, check.0, check.1
                ));
            }
            Some(known) => known.checks.push(check),
            None => rules.push(Rule {
                name: r.name,
                checks: vec![check],
                max_age,
            }),
        }
    }
    Ok(rules)
}

/// The rules of the stored quality documents among `docs`, by Tenant; a
/// stored document that no longer parses is skipped with a warning, so the
/// sweep marks nothing for that Tenant rather than guess.
pub(super) fn load<'a>(docs: impl Iterator<Item = (&'a str, &'a Value)>) -> Rules {
    let mut rules = Rules::new();
    for (tenant, doc) in docs {
        let body = doc.get("text").and_then(Value::as_str).unwrap_or_default();
        match TenantId::new(tenant)
            .map_err(|e| e.to_string())
            .and_then(|t| Ok((t, parse(body.as_bytes())?)))
        {
            Ok(rule) => rules.push(rule),
            Err(e) => tracing::warn!(tenant, error = %e, "quality document skipped"),
        }
    }
    rules
}

/// What one sweep changed for one rule of one Tenant.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Changed {
    /// The Tenant.
    pub tenant: String,
    /// The rule's expanded types, comma-separated.
    pub type_iri: String,
    /// The rule's name.
    pub rule: String,
    /// `quarantine` (marked) or `lift` (the mark removed).
    pub outcome: &'static str,
    /// How many Entities.
    pub count: u64,
}

/// The instances of an internal attribute: an array of instance objects, or
/// one object.
fn instances(attribute: Option<&Value>) -> Vec<&Value> {
    match attribute {
        Some(Value::Array(all)) => all.iter().collect(),
        Some(one @ Value::Object(_)) => vec![one],
        _ => Vec::new(),
    }
}

/// Whether, for one of `rule`'s types the Entity has, some instance of that
/// type's attribute was observed before `cut`. An instance without a
/// readable `observedAt` is not judged: nothing says it is old.
fn stale(doc: &Value, rule: &Rule, cut: chrono::DateTime<chrono::Utc>) -> bool {
    let types = doc.get("type");
    let has = |ty: &str| match types {
        Some(Value::Array(all)) => all.iter().any(|t| t.as_str() == Some(ty)),
        Some(Value::String(one)) => one == ty,
        _ => false,
    };
    rule.checks
        .iter()
        .filter(|(ty, _)| has(ty))
        .flat_map(|(_, attribute)| instances(doc.get(attribute)))
        .any(|inst| {
            inst.get("observedAt")
                .and_then(Value::as_str)
                // A comma may stand for the fraction's point (ISO 8601).
                .and_then(|s| chrono::DateTime::parse_from_rfc3339(&s.replacen(',', ".", 1)).ok())
                .is_some_and(|at| at < cut)
        })
}

/// The rule names an Entity's `quarantineRules` holds.
fn marked_by(doc: &Value) -> Vec<String> {
    instances(doc.get(QUARANTINE_RULES))
        .first()
        .and_then(|inst| inst.get("value"))
        .map(|v| match v {
            Value::Array(names) => names
                .iter()
                .filter_map(Value::as_str)
                .map(str::to_owned)
                .collect(),
            Value::String(one) => vec![one.clone()],
            _ => Vec::new(),
        })
        .unwrap_or_default()
}

/// One Property instance as the write path stores it, stamped at `ts`.
fn property(value: &Value, ts: &str) -> Value {
    json!([{ "type": "Property", "value": value, "createdAt": ts, "modifiedAt": ts }])
}

/// Mark or lift `rule` on `doc` as `cut` judges it now; `false` when the
/// Entity already says what the rule says, so nothing is written.
fn judge(doc: &mut Value, rule: &Rule, cut: chrono::DateTime<chrono::Utc>, ts: &str) -> bool {
    let is_stale = stale(doc, rule, cut);
    let mut names = marked_by(doc);
    let was = names.contains(&rule.name);
    if is_stale == was {
        return false;
    }
    let Some(object) = doc.as_object_mut() else {
        return false;
    };
    if is_stale {
        names.push(rule.name.clone());
        if !object.contains_key(QUARANTINED_AT) {
            object.insert(
                QUARANTINED_AT.to_owned(),
                property(&json!({ "@type": "DateTime", "@value": ts }), ts),
            );
        }
    } else {
        names.retain(|n| *n != rule.name);
    }
    if names.is_empty() {
        object.remove(QUARANTINED_AT);
        object.remove(QUARANTINE_RULES);
    } else {
        object.insert(QUARANTINE_RULES.to_owned(), property(&json!(names), ts));
    }
    true
}

/// One sweep at `now` over the rules the last reload found. Logs and counts
/// how many Entities each rule marked or lifted, never which.
pub(super) async fn sweep(r: &Reloader, now: chrono::DateTime<chrono::Utc>) -> Vec<Changed> {
    let rules = r.quality_rules();
    let mut out = Vec::new();
    for (t, list) in rules.iter() {
        for rule in list {
            for changed in judge_rule(&r.state, t, rule, now).await {
                metrics::counter!("antares_quality_sweep_total",
                    "tenant" => changed.tenant.clone(), "type" => changed.type_iri.clone(),
                    "rule" => changed.rule.clone(), "outcome" => changed.outcome)
                .increment(changed.count);
                tracing::info!(
                    tenant = changed.tenant,
                    r#type = changed.type_iri,
                    rule = changed.rule,
                    outcome = changed.outcome,
                    count = changed.count,
                    "quality sweep"
                );
                out.push(changed);
            }
        }
    }
    out
}

/// The Entities of `rule`'s types in Tenant `t` whose mark the rule would
/// change at `now`, read in at most [`ROUNDS`] pages of [`PAGE`]; then each
/// one marked or lifted under its row lock, judged again on what the store
/// holds at that moment.
async fn judge_rule(
    state: &AppState,
    t: &TenantId,
    rule: &Rule,
    now: chrono::DateTime<chrono::Utc>,
) -> Vec<Changed> {
    let tenant = t.as_str();
    let mut types: Vec<String> = Vec::new();
    let mut attrs = vec![QUARANTINE_RULES.to_owned()];
    for (ty, attribute) in &rule.checks {
        if !types.contains(ty) {
            types.push(ty.clone());
        }
        if !attrs.contains(attribute) {
            attrs.push(attribute.clone());
        }
    }
    let type_iri = types.join(",");
    let failed = |e: &str| {
        tracing::warn!(tenant, r#type = type_iri.as_str(), rule = rule.name.as_str(), error = %e, "quality sweep failed");
    };
    // A maxAge holds at most ten years, so the cut is always in range.
    let cut = chrono::Duration::from_std(rule.max_age)
        .ok()
        .and_then(|age| now.checked_sub_signed(age))
        .unwrap_or(chrono::DateTime::<chrono::Utc>::MIN_UTC);
    let ts = now.to_rfc3339_opts(chrono::SecondsFormat::Millis, true);
    let mut pending: Vec<String> = Vec::new();
    for round in 0..ROUNDS {
        let rows = match state
            .scan_entities(t, &types, &attrs, round.saturating_mul(PAGE), PAGE)
            .await
        {
            Ok(rows) => rows,
            Err(e) => {
                failed(&e);
                return Vec::new();
            }
        };
        let last = rows.len() < PAGE;
        pending.extend(rows.iter().filter_map(|doc| {
            let was = marked_by(doc).contains(&rule.name);
            (stale(doc, rule, cut) != was)
                .then(|| doc.get("id").and_then(Value::as_str).map(str::to_owned))
                .flatten()
        }));
        if last {
            break;
        }
    }
    let (mut marked, mut lifted) = (0u64, 0u64);
    for batch in pending.chunks(PAGE) {
        // Whether each id the closure changed was marked (`true`) or lifted.
        let mut marking = std::collections::HashMap::new();
        let written = state
            .mutate_entities(t, batch, |id, doc| {
                let is_stale = stale(doc, rule, cut);
                let changed = judge(doc, rule, cut, &ts);
                if changed {
                    marking.insert(id.to_owned(), is_stale);
                }
                changed
            })
            .await;
        match written {
            Ok(written) => {
                for (id, done) in batch.iter().zip(written) {
                    match (done, marking.get(id)) {
                        (true, Some(true)) => marked = marked.saturating_add(1),
                        (true, Some(false)) => lifted = lifted.saturating_add(1),
                        _ => {}
                    }
                }
            }
            Err(e) => {
                failed(&e);
                break;
            }
        }
    }
    [("quarantine", marked), ("lift", lifted)]
        .into_iter()
        .filter(|(_, count)| *count > 0)
        .map(|(outcome, count)| Changed {
            tenant: tenant.to_owned(),
            type_iri: type_iri.clone(),
            rule: rule.name.clone(),
            outcome,
            count,
        })
        .collect()
}

/// `/q/quality/{tenant}`: GET the document as written, PUT one, DELETE it.
pub(super) fn routes(weak: &Weak<Reloader>) -> Router {
    let (read, write, remove) = (weak.clone(), weak.clone(), weak.clone());
    Router::new().route(
        "/quality/{tenant}",
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
        .map(|t| format!("{QUALITY_ID_PREFIX}{}", t.as_str()))
}

fn none() -> Response {
    problem(
        StatusCode::NOT_FOUND,
        "ResourceNotFound",
        "no quality document for this tenant",
    )
}

async fn get_doc(r: Arc<Reloader>, tenant: String) -> Result<Response, String> {
    let Some(key) = key(&tenant) else {
        return Ok(bad_tenant());
    };
    Ok(
        match r
            .state
            .view_doc(&key)
            .await?
            .as_ref()
            .and_then(|d| d.get("text"))
            .and_then(Value::as_str)
        {
            Some(text) => (
                [(axum::http::header::CONTENT_TYPE, "application/json")],
                text.to_owned(),
            )
                .into_response(),
            None => none(),
        },
    )
}

async fn put_doc(r: Arc<Reloader>, tenant: String, body: Bytes) -> Result<Response, String> {
    let Some(key) = key(&tenant) else {
        return Ok(bad_tenant());
    };
    if body.len() > MAX_QUALITY_BYTES {
        return Ok(problem(
            StatusCode::PAYLOAD_TOO_LARGE,
            "BadRequestData",
            &format!("a quality document is at most {MAX_QUALITY_BYTES} bytes"),
        ));
    }
    if let Err(why) = parse(&body) {
        return Ok(problem(StatusCode::BAD_REQUEST, "BadRequestData", &why));
    }
    let text = String::from_utf8_lossy(&body).into_owned();
    let replaced = r
        .state
        .put_view_doc(&key, json!({"id": key, "text": text}))
        .await?;
    r.reload().await?;
    Ok(if replaced {
        StatusCode::NO_CONTENT
    } else {
        StatusCode::CREATED
    }
    .into_response())
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

    const DOC: &[u8] = br#"{"rules":[{"name":"stale-position","type":"https://e.org/Vehicle",
        "attribute":"https://e.org/location","maxAge":"PT10M"}]}"#;

    fn rule() -> Rule {
        parse(DOC)
            .unwrap_or_default()
            .into_iter()
            .next()
            .unwrap_or_else(|| Rule {
                name: String::new(),
                checks: Vec::new(),
                max_age: Duration::ZERO,
            })
    }

    #[test]
    fn a_document_names_labels_and_expanded_iris_once() {
        let r = rule();
        assert_eq!(r.name, "stale-position");
        assert_eq!(r.max_age, Duration::from_secs(600));
        assert!(parse(br#"{"rules":[{"name":"a","type":"https://e.org/V","attribute":"https://e.org/x","maxAge":"PT1S"}]}"#).is_ok());
        for bad in [
            &br#"{"rules":[{"name":"A","type":"https://e.org/V","attribute":"https://e.org/x","maxAge":"PT1M"}]}"#[..],
            br#"{"rules":[{"name":"a","type":"Vehicle","attribute":"https://e.org/x","maxAge":"PT1M"}]}"#,
            br#"{"rules":[{"name":"a","type":"https://e.org/V","attribute":"speed","maxAge":"PT1M"}]}"#,
            br#"{"rules":[{"name":"a","type":"https://e.org/V","attribute":"https://e.org/x","maxAge":"PT0S"}]}"#,
            br#"{"rules":[{"name":"a","type":"https://e.org/V","attribute":"https://e.org/x","maxAge":"P3651D"}]}"#,
            br#"{"rules":[{"name":"a","type":"https://e.org/V","attribute":"https://e.org/x","maxAge":"PT1M"},
                          {"name":"a","type":"https://e.org/V","attribute":"https://e.org/x","maxAge":"PT1M"}]}"#,
            br#"{"rules":[{"name":"a","type":"https://e.org/V","attribute":"https://e.org/x","maxAge":"PT1M"},
                          {"name":"a","type":"https://e.org/S","attribute":"https://e.org/x","maxAge":"PT2M"}]}"#,
            br#"{"rules":[{"name":"a","type":"https://e.org/V","attribute":"https://e.org/x","maxAge":"PT1M","outcome":"flag"}]}"#,
            br#"{"rules":[{"name":"-a","type":"https://e.org/V","attribute":"https://e.org/x","maxAge":"PT1M"}]}"#,
            b"not json",
        ] {
            assert!(parse(bad).is_err(), "{}", String::from_utf8_lossy(bad));
        }
    }

    #[test]
    fn a_stale_entity_is_marked_once_and_lifted_when_fresh() {
        let r = rule();
        let now = chrono::Utc::now();
        let cut = now - chrono::Duration::minutes(10);
        let at = |minutes: i64| {
            (now - chrono::Duration::minutes(minutes))
                .to_rfc3339_opts(chrono::SecondsFormat::Millis, true)
        };
        let mut doc = json!({"id": "urn:v:1", "type": ["https://e.org/Vehicle"],
            "https://e.org/location": [{"type": "Property", "value": 1, "observedAt": at(11)}]});
        assert!(judge(&mut doc, &r, cut, "t1"));
        assert_eq!(marked_by(&doc), ["stale-position"]);
        assert!(doc.get(QUARANTINED_AT).is_some());
        assert!(
            !judge(&mut doc, &r, cut, "t2"),
            "marked twice is marked once"
        );

        doc["https://e.org/location"] =
            json!([{"type": "Property", "value": 2, "observedAt": at(1)}]);
        assert!(judge(&mut doc, &r, cut, "t3"));
        assert!(doc.get(QUARANTINED_AT).is_none() && doc.get(QUARANTINE_RULES).is_none());

        // Another rule's mark stays when this one lifts; the attribute gone lifts too.
        doc[QUARANTINE_RULES] = property(&json!(["stale-position", "other"]), "t4");
        doc[QUARANTINED_AT] = property(&json!({"@type": "DateTime", "@value": "t4"}), "t4");
        doc.as_object_mut()
            .map(|o| o.remove("https://e.org/location"));
        assert!(judge(&mut doc, &r, cut, "t5"));
        assert_eq!(marked_by(&doc), ["other"]);
        assert!(doc.get(QUARANTINED_AT).is_some());
    }

    #[test]
    fn the_entries_of_one_name_are_one_rule() {
        let rules = parse(
            br#"{"rules":[
            {"name":"a","type":"https://e.org/V","attribute":"https://e.org/x","maxAge":"PT1M"},
            {"name":"b","type":"https://e.org/V","attribute":"https://e.org/x","maxAge":"PT5M"},
            {"name":"a","type":"https://e.org/S","attribute":"https://e.org/x","maxAge":"PT1M"},
            {"name":"a","type":"https://e.org/V","attribute":"https://e.org/y","maxAge":"PT1M"}]}"#,
        )
        .unwrap_or_default();
        let shape: Vec<(&str, usize)> = rules
            .iter()
            .map(|r| (r.name.as_str(), r.checks.len()))
            .collect();
        assert_eq!(shape, [("a", 3), ("b", 1)]);
    }

    /// An Entity of two types one name judges: the stale attribute of one
    /// type marks it, the fresh one of the other does not lift it, so two
    /// entries of a name never undo each other sweep after sweep.
    #[test]
    fn two_entries_of_one_name_never_undo_each_other() {
        let r = parse(br#"{"rules":[
            {"name":"a","type":"https://e.org/V","attribute":"https://e.org/x","maxAge":"PT10M"},
            {"name":"a","type":"https://e.org/S","attribute":"https://e.org/y","maxAge":"PT10M"}]}"#)
        .unwrap_or_default()
        .into_iter()
        .next()
        .unwrap_or_else(rule);
        let now = chrono::Utc::now();
        let cut = now - chrono::Duration::minutes(10);
        let at = |minutes: i64| {
            (now - chrono::Duration::minutes(minutes))
                .to_rfc3339_opts(chrono::SecondsFormat::Millis, true)
        };
        let mut doc = json!({"id": "urn:v:1", "type": ["https://e.org/V", "https://e.org/S"],
            "https://e.org/x": [{"type": "Property", "value": 1, "observedAt": at(30)}],
            "https://e.org/y": [{"type": "Property", "value": 1, "observedAt": at(1)}]});
        assert!(judge(&mut doc, &r, cut, "t1"));
        assert!(!judge(&mut doc, &r, cut, "t2"), "the mark stays");
        // The attribute of a type the Entity lacks is not judged.
        let mut other = json!({"id": "urn:s:1", "type": ["https://e.org/S"],
            "https://e.org/x": [{"type": "Property", "value": 1, "observedAt": at(30)}]});
        assert!(!judge(&mut other, &r, cut, "t3"));
    }

    #[test]
    fn an_instance_without_observed_at_is_not_judged() {
        let r = rule();
        let mut doc = json!({"id": "urn:v:1", "type": ["https://e.org/Vehicle"], "https://e.org/location": [{"type": "Property", "value": 1}]});
        assert!(!judge(&mut doc, &r, chrono::Utc::now(), "t"));
    }
}
