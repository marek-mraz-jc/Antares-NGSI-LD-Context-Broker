// SPDX-License-Identifier: EUPL-1.2
//! The rule document of one Tenant (README section 2), parsed and compiled
//! once at load: every name expanded, every query parsed, every pattern
//! compiled, so a decision only compares.

use antares_api::policy::{Rule, TimeBound, Window};
use antares_ql::geo::GeoQuery;
use chrono::{DateTime, Duration, Utc};
use serde::Deserialize;
use std::collections::{BTreeSet, HashMap};
use std::sync::Arc;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Document {
    policies: Vec<PolicyDoc>,
    /// The policies each listed view is decided by (README section 2).
    #[serde(default)]
    views: HashMap<String, Vec<String>>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
struct PolicyDoc {
    id: String,
    #[serde(default)]
    effect: Effect,
    #[serde(default)]
    assigner: Option<String>,
    assignee: Assignee,
    operations: Vec<String>,
    #[serde(default)]
    validity: Option<Validity>,
    #[serde(default)]
    information: Vec<Information>,
    #[serde(default)]
    q: Option<String>,
    #[serde(default)]
    scope_q: Option<String>,
    #[serde(default)]
    geo_q: Option<String>,
    #[serde(default)]
    temporal_q: Option<String>,
}

#[derive(Deserialize, Default, PartialEq, Eq, Clone, Copy)]
#[serde(rename_all = "lowercase")]
enum Effect {
    #[default]
    Permit,
    Prohibit,
}

/// Who a policy is about.
#[derive(Deserialize, Debug, Clone, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Assignee {
    /// What `id` names.
    pub kind: Kind,
    /// The user, group, role, service account or DID.
    pub id: String,
}

/// The kinds of assignee (platform `PrincipalKind`).
#[derive(Deserialize, Debug, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub enum Kind {
    /// `x-jc-subject`.
    User,
    /// One of `x-jc-groups`.
    Group,
    /// One of `x-jc-roles`.
    Role,
    /// `x-jc-client`.
    ServiceAccount,
    /// No header carries one yet: matches nobody.
    Did,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Validity {
    #[serde(default)]
    from: Option<DateTime<Utc>>,
    #[serde(default)]
    to: Option<DateTime<Utc>>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
struct Information {
    #[serde(default)]
    entities: Vec<SelectorDoc>,
    #[serde(default)]
    property_names: Vec<String>,
    #[serde(default)]
    relationship_names: Vec<String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
struct SelectorDoc {
    #[serde(rename = "type")]
    entity_type: String,
    #[serde(default)]
    id: Option<String>,
    #[serde(default)]
    id_pattern: Option<String>,
}

/// One compiled policy.
#[derive(Debug, Clone)]
pub struct Policy {
    /// Its `id`, for a refusal's reason and the access surface.
    pub id: String,
    /// A prohibition rather than a permission.
    pub prohibit: bool,
    /// Who it is about.
    pub assignee: Assignee,
    /// The CIM 009 clauses it speaks about.
    pub clauses: BTreeSet<String>,
    /// `[from, to)`; an open end is unbounded.
    pub validity: (Option<DateTime<Utc>>, Option<DateTime<Utc>>),
    /// One rule per selector: its type, id or pattern and the Attributes its
    /// information shows, with the policy's conditions. Empty = every type.
    pub selectors: Vec<Rule>,
    /// The policy's conditions as one rule without type over every
    /// Attribute, for a policy that names no selector: [`Rule::types`] is
    /// filled per operation.
    pub conditions: Rule,
    /// The policy as written, for the access surface.
    pub source: Source,
}

/// A policy as its document wrote it (README section 4): names before
/// expansion and conditions as strings, so the access surface shows the
/// caller the words their grant was given in. Never read by a decision.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Source {
    /// The party that granted it.
    pub assigner: Option<String>,
    /// One per `information.entities` member, in document order.
    pub selectors: Vec<Selector>,
    /// `q`.
    pub q: Option<String>,
    /// `scopeQ`.
    pub scope_q: Option<String>,
    /// `geoQ`.
    pub geo_q: Option<String>,
    /// `temporalQ`.
    pub temporal_q: Option<String>,
}

/// One Entity selector as written, with its information's Attributes.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Selector {
    /// `type`.
    pub entity_type: String,
    /// `id`.
    pub id: Option<String>,
    /// `idPattern`.
    pub id_pattern: Option<String>,
    /// `propertyNames` and `relationshipNames`, sorted; empty = the whole
    /// Entity.
    pub attributes: Vec<String>,
}

impl Policy {
    /// Whether the policy is in force at `now`.
    #[must_use]
    pub fn in_force(&self, now: DateTime<Utc>) -> bool {
        self.validity.0.is_none_or(|from| now >= from) && self.validity.1.is_none_or(|to| now < to)
    }

    /// Whether the policy sets a condition on the Entity or its history.
    #[must_use]
    pub fn is_conditional(&self) -> bool {
        self.conditions.is_conditional() || !self.conditions.temporal.is_empty()
    }
}

/// One Tenant's compiled document.
#[derive(Debug, Clone, Default)]
pub struct Table {
    /// In document order.
    pub policies: Vec<Policy>,
    /// Per listed view, the table of its policies alone, in document order.
    pub views: HashMap<String, Arc<Table>>,
}

impl Table {
    /// The table a request through `view` is decided by: the view's own
    /// when the document lists it, this one otherwise.
    #[must_use]
    pub fn through(&self, view: Option<&str>) -> &Table {
        view.and_then(|v| self.views.get(v)).map_or(self, |t| t)
    }
}

/// The core `@context` every name is expanded by: the broker hands the
/// engine expanded names, and an IRI expands to itself.
pub fn context() -> &'static antares_jsonld::Context {
    static CORE: std::sync::OnceLock<antares_jsonld::Context> = std::sync::OnceLock::new();
    CORE.get_or_init(antares_jsonld::core_context)
}

/// Compile `doc`, or say why it is refused. `now` resolves nothing: a
/// relative `temporalQ` stays relative ([`TimeBound::Ago`]).
///
/// # Errors
/// The first member that does not parse, named by its policy.
pub fn compile(doc: &serde_json::Value) -> Result<Table, String> {
    let doc: Document =
        Document::deserialize(doc).map_err(|e| format!("the rule document: {e}"))?;
    let mut seen = BTreeSet::new();
    let mut policies = Vec::with_capacity(doc.policies.len());
    for p in doc.policies {
        if !seen.insert(p.id.clone()) {
            return Err(format!("policy {:?} is named twice", p.id));
        }
        policies.push(policy(p)?);
    }
    let mut views = HashMap::with_capacity(doc.views.len());
    for (view, ids) in doc.views {
        let mut listed = BTreeSet::new();
        for id in &ids {
            if !seen.contains(id) {
                return Err(format!(
                    "view {view:?} names policy {id:?}, which is not in the document"
                ));
            }
            if !listed.insert(id.as_str()) {
                return Err(format!("view {view:?} names policy {id:?} twice"));
            }
        }
        let own = policies
            .iter()
            .filter(|p| listed.contains(p.id.as_str()))
            .cloned()
            .collect();
        views.insert(
            view,
            Arc::new(Table {
                policies: own,
                views: HashMap::new(),
            }),
        );
    }
    Ok(Table { policies, views })
}

fn policy(p: PolicyDoc) -> Result<Policy, String> {
    let id = p.id;
    let why = |what: String| format!("policy {id:?}: {what}");
    if id.is_empty() {
        return Err("a policy has an empty id".into());
    }
    if p.operations.is_empty() {
        return Err(why("names no operation".into()));
    }
    let q =
        p.q.as_deref()
            .map(|q| {
                let node = antares_ql::parse_q(q).map_err(|e| why(format!("q {q:?}: {e}")))?;
                if node.has_linked_paths() {
                    return Err(why(format!(
                        "q {q:?} reads a linked Entity, which a rule cannot follow"
                    )));
                }
                Ok(node)
            })
            .transpose()?;
    let geo = p
        .geo_q
        .as_deref()
        .map(|g| geo_query(g).map_err(|e| why(format!("geoQ {g:?}: {e}"))))
        .transpose()?;
    let temporal = p
        .temporal_q
        .as_deref()
        .map(|t| window(t).map_err(|e| why(format!("temporalQ {t:?}: {e}"))))
        .transpose()?;
    // a granted path covers its subtree (platform R13, R30): `/a/b` is the
    // scope query `/a/b/#`, which matches `/a/b` and below, never `/a/bc`
    let scope_q = match p.scope_q.as_deref() {
        None => None,
        Some(s) => {
            let tree: Vec<String> = scope_paths(s).map(|p| format!("{p}/#")).collect();
            if tree.is_empty() {
                return Err(why(format!("scopeQ {s:?} names no scope path")));
            }
            Some(tree.join(","))
        }
    };
    let (from, to) = p.validity.map_or((None, None), |v| (v.from, v.to));
    let mut source = Source {
        assigner: p.assigner,
        selectors: Vec::new(),
        q: p.q,
        scope_q: p.scope_q,
        geo_q: p.geo_q,
        temporal_q: p.temporal_q,
    };
    let conditions = Rule {
        every_attribute: true,
        q: q.map(Box::new),
        scope_q,
        geo_q: geo.map(Box::new),
        temporal: temporal.into_iter().collect(),
        ..Rule::default()
    };
    let (selectors, written) = rules(p.information, &conditions, &why)?;
    source.selectors = written;
    Ok(Policy {
        id,
        prohibit: p.effect == Effect::Prohibit,
        assignee: p.assignee,
        clauses: p.operations.into_iter().collect(),
        validity: (from, to),
        selectors,
        conditions,
        source,
    })
}

/// One rule per Entity selector of `information`, with its information's
/// Attributes and the policy's `conditions`, and the selectors as written.
fn rules(
    information: Vec<Information>,
    conditions: &Rule,
    why: &dyn Fn(String) -> String,
) -> Result<(Vec<Rule>, Vec<Selector>), String> {
    let ctx = context();
    let mut selectors = Vec::new();
    let mut written_selectors = Vec::new();
    for info in information {
        let attributes: Vec<String> = info
            .property_names
            .iter()
            .chain(&info.relationship_names)
            .map(|n| ctx.expand_key(n))
            .collect();
        if info.entities.is_empty() {
            return Err(why("an information entry selects no Entity".into()));
        }
        let mut written: Vec<String> = info
            .property_names
            .iter()
            .chain(&info.relationship_names)
            .cloned()
            .collect();
        written.sort();
        written.dedup();
        for s in info.entities {
            if s.id.is_some() && s.id_pattern.is_some() {
                return Err(why(format!(
                    "the selector of {:?} names both id and idPattern",
                    s.entity_type
                )));
            }
            if let Some(p) = s.id_pattern.as_deref() {
                antares_ql::regex::compile(&format!("^(?:{p})$"))
                    .map_err(|e| why(format!("idPattern {p:?}: {e}")))?;
            }
            written_selectors.push(Selector {
                entity_type: s.entity_type.clone(),
                id: s.id.clone(),
                id_pattern: s.id_pattern.clone(),
                attributes: written.clone(),
            });
            selectors.push(Rule {
                types: vec![ctx.expand_key(&s.entity_type)],
                ids: s.id.into_iter().collect(),
                id_patterns: s.id_pattern.into_iter().collect(),
                // no list is the whole Entity (EP-16, map K9)
                every_attribute: attributes.is_empty(),
                attributes: attributes.clone(),
                ..conditions.clone()
            });
        }
    }
    Ok((selectors, written_selectors))
}

/// `georel=…;geometry=…;coordinates=…[;geoproperty=…]` as the broker parses
/// a request's geoquery.
fn geo_query(raw: &str) -> Result<GeoQuery, String> {
    let params: HashMap<String, String> = raw
        .split(';')
        .filter_map(|part| part.split_once('='))
        .map(|(k, v)| (k.trim().to_owned(), v.trim().to_owned()))
        .collect();
    GeoQuery::from_params(&params)
        .map_err(|e| e.to_string())?
        .ok_or_else(|| "no georel".to_owned())
}

/// `timerel=before|after|between;timeAt=…[;endTimeAt=…]`, each instant RFC
/// 3339 or a duration before now (`P-1D`, `-P1D`).
fn window(raw: &str) -> Result<Window, String> {
    let (mut rel, mut at, mut end) = (None, None, None);
    for part in raw.split([';', '&']) {
        match part.split_once('=').map(|(k, v)| (k.trim(), v.trim())) {
            Some(("timerel", v)) => rel = Some(v),
            Some(("timeAt", v)) => at = Some(bound(v)?),
            Some(("endTimeAt", v)) => end = Some(bound(v)?),
            _ => return Err(format!("{part:?} is not timerel, timeAt or endTimeAt")),
        }
    }
    let need = |b: Option<TimeBound>, name: &str| b.ok_or_else(|| format!("no {name}"));
    match rel {
        Some("after") => Ok(Window {
            from: Some(need(at, "timeAt")?),
            to: None,
        }),
        Some("before") => Ok(Window {
            from: None,
            to: Some(need(at, "timeAt")?),
        }),
        Some("between") => Ok(Window {
            from: Some(need(at, "timeAt")?),
            to: Some(need(end, "endTimeAt")?),
        }),
        other => Err(format!("timerel {other:?} is not before, after or between")),
    }
}

fn bound(raw: &str) -> Result<TimeBound, String> {
    if let Ok(at) = DateTime::parse_from_rfc3339(raw) {
        return Ok(TimeBound::At(at.with_timezone(&Utc)));
    }
    match duration(raw) {
        Some(d) if d < Duration::zero() => Ok(TimeBound::Ago(d.abs())),
        // zero is now, which no window reaches back to
        Some(_) => Err(format!("{raw:?} does not lie in the past")),
        None => Err(format!(
            "{raw:?} is neither an instant nor a duration in weeks, days, hours, minutes and seconds"
        )),
    }
}

/// An ISO 8601 duration as a signed offset (`P-1D` and `-P1D` are a day
/// ago); months and years have no fixed length and are refused.
fn duration(raw: &str) -> Option<Duration> {
    let (mut sign, rest) = match raw.strip_prefix('-') {
        Some(rest) => (-1i32, rest),
        None => (1i32, raw),
    };
    let rest = rest.strip_prefix('P')?;
    let (date, time) = rest.split_once('T').unwrap_or((rest, ""));
    let mut total = Duration::zero();
    let mut digits = String::new();
    let mut units_seen = false;
    for (part, units) in [(date, "WD"), (time, "HMS")] {
        for c in part.chars() {
            match c {
                '-' if digits.is_empty() => sign = sign.checked_neg()?,
                '0'..='9' => digits.push(c),
                u if units.contains(u) => {
                    let n: i64 = digits.parse().ok()?;
                    digits.clear();
                    units_seen = true;
                    total = total.checked_add(&match u {
                        'W' => Duration::try_weeks(n)?,
                        'D' => Duration::try_days(n)?,
                        'H' => Duration::try_hours(n)?,
                        'M' => Duration::try_minutes(n)?,
                        _ => Duration::try_seconds(n)?,
                    })?;
                }
                _ => return None,
            }
        }
    }
    (digits.is_empty() && units_seen).then(|| total.checked_mul(sign))?
}

/// The scope paths of a `scopeQ`, whose operators separate them; a trailing
/// `/#` or `/` names the same subtree.
pub fn scope_paths(scope_q: &str) -> impl Iterator<Item = &str> {
    scope_q
        .split(['(', ')', ';', '|', ','])
        .map(str::trim)
        .filter(|p| p.starts_with('/'))
        .map(|p| p.trim_end_matches("/#").trim_end_matches('/'))
        .filter(|p| !p.is_empty())
}

/// Whether the granted path covers `scope` as a path: `/a/b` covers `/a/b`
/// and `/a/b/c`, never `/a/bc`.
#[must_use]
pub fn covers(granted: &str, scope: &str) -> bool {
    scope == granted
        || scope
            .strip_prefix(granted)
            .is_some_and(|rest| rest.starts_with('/'))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn one(p: &serde_json::Value) -> Result<Policy, String> {
        compile(&json!({ "policies": [p] }))?
            .policies
            .into_iter()
            .next()
            .ok_or_else(|| "no policy".to_owned())
    }

    fn base() -> serde_json::Value {
        json!({"id": "p", "assignee": {"kind": "role", "id": "public"}, "operations": ["5.7.2"]})
    }

    fn with(k: &str, v: serde_json::Value) -> serde_json::Value {
        let mut p = base();
        if let Some(o) = p.as_object_mut() {
            o.insert(k.to_owned(), v);
        }
        p
    }

    #[test]
    fn a_policy_compiles_one_rule_per_selector_with_its_conditions() -> Result<(), String> {
        let p = one(&with(
            "information",
            json!([{"entities": [{"type": "Car"}, {"type": "Bus", "id": "urn:ngsi-ld:Bus:1"}],
                    "propertyNames": ["speed"], "relationshipNames": ["ownedBy"]}]),
        ))?;
        let ctx = context();
        assert_eq!(p.selectors.len(), 2);
        let car = p.selectors.first().ok_or("no Car selector")?;
        let bus = p.selectors.get(1).ok_or("no Bus selector")?;
        assert_eq!(car.types, [ctx.expand_key("Car")]);
        assert_eq!(bus.ids, ["urn:ngsi-ld:Bus:1"]);
        assert_eq!(
            car.attributes,
            [ctx.expand_key("speed"), ctx.expand_key("ownedBy")]
        );
        assert!(!p.prohibit && !p.is_conditional());
        Ok(())
    }

    #[test]
    fn every_malformed_member_is_refused_by_name() {
        let cases = [
            (json!({"policies": [base(), base()]}), "named twice"),
            (json!({"policies": [with("id", json!(""))]}), "empty id"),
            (
                json!({"policies": [with("operations", json!([]))]}),
                "names no operation",
            ),
            (
                json!({"policies": [with("q", json!("speed>"))]}),
                "q \"speed>\"",
            ),
            (
                json!({"policies": [with("q", json!("a{b}==1"))]}),
                "linked Entity",
            ),
            (
                json!({"policies": [with("geoQ", json!("georel=near"))]}),
                "geoQ",
            ),
            (
                json!({"policies": [with("geoQ", json!("geometry=Point"))]}),
                "geoQ",
            ),
            (
                json!({"policies": [with("temporalQ", json!("timerel=after"))]}),
                "no timeAt",
            ),
            (
                json!({"policies": [with("temporalQ", json!("timerel=between;timeAt=P-1D"))]}),
                "no endTimeAt",
            ),
            (
                json!({"policies": [with("temporalQ", json!("timerel=during;timeAt=P-1D"))]}),
                "is not before",
            ),
            (
                json!({"policies": [with("temporalQ", json!("timerel=after;timeAt=P1D"))]}),
                "past",
            ),
            (
                json!({"policies": [with("temporalQ", json!("timerel=after;timeAt=P1Y"))]}),
                "neither",
            ),
            (
                json!({"policies": [with("temporalQ", json!("x=1"))]}),
                "is not timerel",
            ),
            (
                json!({"policies": [with("temporalQ", json!("timerel=before"))]}),
                "no timeAt",
            ),
            (
                json!({"policies": [with("temporalQ", json!("timerel=between;timeAt=P-2D;endTimeAt=soon"))]}),
                "soon",
            ),
            (
                json!({"policies": [with("temporalQ", json!("timerel=after;timeAt=P-99999999999999999999D"))]}),
                "P-99999999999999999999D",
            ),
            (
                json!({"policies": [with("geoQ", json!("near"))]}),
                "no georel",
            ),
            (
                json!({"policies": [with("scopeQ", json!("nope"))]}),
                "names no scope path",
            ),
            (
                json!({"policies": [with("information", json!([{"entities": []}]))]}),
                "selects no Entity",
            ),
            (
                json!({"policies": [with("information", json!([{"entities": [{"type": "Car", "id": "urn:a", "idPattern": "x"}]}]))]}),
                "both id and idPattern",
            ),
            (
                json!({"policies": [with("information", json!([{"entities": [{"type": "Car", "idPattern": "("}]}]))]}),
                "idPattern",
            ),
            (
                json!({"policies": [with("extra", json!(1))]}),
                "unknown field",
            ),
            (json!({"rules": []}), "unknown field"),
            (
                json!({"policies": [with("assignee", json!({"kind": "team", "id": "x"}))]}),
                "unknown variant",
            ),
        ];
        for (doc, said) in cases {
            let why = compile(&doc).err().unwrap_or_default();
            assert!(why.contains(said), "{doc}: {why:?} does not say {said:?}");
        }
    }

    /// T-3729: names expand by the core `@context`, the form the broker
    /// hands the engine; a policy's own conditions show every Attribute;
    /// an `idPattern` selects by its pattern.
    #[test]
    fn a_selector_keeps_its_id_pattern_and_the_conditions_every_attribute() -> Result<(), String> {
        assert_eq!(
            context().expand_key("location"),
            "https://uri.etsi.org/ngsi-ld/location"
        );
        let p = one(&with(
            "information",
            json!([{"entities": [{"type": "Car", "idPattern": "urn:ngsi-ld:Car:a.*"}]}]),
        ))?;
        assert!(p.conditions.every_attribute);
        let car = p.selectors.first().ok_or("no Car selector")?;
        assert_eq!(car.id_patterns, ["urn:ngsi-ld:Car:a.*"]);
        assert!(car.selects_id("urn:ngsi-ld:Car:a1") && !car.selects_id("urn:ngsi-ld:Car:b1"));
        Ok(())
    }

    #[test]
    fn conditions_and_validity_compile() -> Result<(), String> {
        let mut p = with("q", json!("speed<100"));
        if let Some(o) = p.as_object_mut() {
            o.insert("scopeQ".into(), json!("/geo/SK/#"));
            o.insert("effect".into(), json!("prohibit"));
            o.insert(
                "geoQ".into(),
                json!(
                    "georel=within;geometry=Polygon;coordinates=[[[0,0],[1,0],[1,1],[0,1],[0,0]]]"
                ),
            );
            o.insert(
                "temporalQ".into(),
                json!("timerel=between;timeAt=2026-01-01T00:00:00Z;endTimeAt=-PT1H"),
            );
            o.insert(
                "validity".into(),
                json!({"from": "2026-01-01T00:00:00Z", "to": "2027-01-01T00:00:00Z"}),
            );
        }
        let p = one(&p)?;
        assert!(p.prohibit && p.is_conditional());
        assert_eq!(
            p.conditions.temporal.first().and_then(|w| w.to),
            Some(TimeBound::Ago(Duration::hours(1)))
        );
        let at = |s: &str| {
            DateTime::parse_from_rfc3339(s)
                .map(|d| d.with_timezone(&Utc))
                .unwrap_or_default()
        };
        assert!(!p.in_force(at("2025-12-31T23:59:59Z")));
        assert!(p.in_force(at("2026-01-01T00:00:00Z")));
        assert!(!p.in_force(at("2027-01-01T00:00:00Z")));
        let only_window = one(&with("temporalQ", json!("timerel=before;timeAt=P-1W")))?;
        assert!(only_window.is_conditional());
        Ok(())
    }

    #[test]
    fn durations_read_as_the_platform_wrote_them() {
        assert_eq!(duration("P-1D"), Some(-Duration::days(1)));
        assert_eq!(duration("-P1D"), Some(-Duration::days(1)));
        assert_eq!(duration("-PT1H30M"), Some(-Duration::minutes(90)));
        assert_eq!(
            duration("P1W2DT3S"),
            Some(Duration::days(9) + Duration::seconds(3))
        );
        // the last sums past what a Duration holds
        for bad in [
            "P",
            "1D",
            "P1",
            "P1M",
            "PT1D",
            "P1-2D",
            "Px",
            "-P100000000000DT2000000000000H",
        ] {
            assert_eq!(duration(bad), None, "{bad}");
        }
        // T-3729: a bound is a duration strictly before now
        assert_eq!(duration("P0D"), Some(Duration::zero()));
        for (raw, ok) in [("P0D", false), ("PT1S", false), ("-PT1S", true)] {
            assert_eq!(bound(raw).is_ok(), ok, "{raw}");
        }
    }

    #[test]
    fn a_scope_path_covers_its_subtree_and_nothing_beside_it() {
        assert_eq!(
            scope_paths("(/a/b/#|/c/);x").collect::<Vec<_>>(),
            ["/a/b", "/c"]
        );
        assert!(covers("/a/b", "/a/b"));
        assert!(covers("/a/b", "/a/b/c"));
        assert!(!covers("/a/b", "/a/bc"));
        assert!(!covers("/a/b", "/a"));
    }
}
