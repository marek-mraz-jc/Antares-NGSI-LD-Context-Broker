// SPDX-License-Identifier: EUPL-1.2
//! The decisions of README section 3: prohibitions first, default deny, a
//! read narrowed to one rule per selector, a write whole inside one policy.

use crate::document::{context, covers, scope_paths, Policy, Table};
use crate::subject::{Caller, Platform};
use antares_api::policy::{
    Decision, Filter, Operation, Rule, Warning, FILTERABLE, WHOLE_TENANT, WRITE_BY_ID,
};
use chrono::{DateTime, Utc};
use serde_json::Value;
use std::collections::{BTreeMap, BTreeSet};

/// Writes judged on the Entity they create, so a conditional policy is
/// judged on the request body.
const CREATES: [&str; 2] = ["5.6.1", "5.6.7"];

/// Access control written into the data (platform GW29): an Entity never
/// carries a member of these names, short or expanded.
const POLICY_VOCABULARY: [&str; 6] = [
    "owner",
    "acl",
    "allowedRoles",
    "visibility",
    "permissions",
    "policy",
];

/// Why an operation no policy grants is refused; the same words for every
/// refusal by the rules, so the reason tells nothing about them (GW6).
pub const NOT_GRANTED: &str = "no policy grants this operation";
/// Why a prohibited operation is refused.
pub const PROHIBITED: &str = "a policy prohibits this operation";
/// Why a read under a policy granting every type with a condition is
/// refused when it names no type: a rule needs a type.
pub const NAME_THE_TYPES: &str = "name the Entity types to read";
/// Why a call from inside the cluster is refused (README section 1).
pub const INTERNAL_ONLY: &str =
    "a request without a subject reaches the Context Source operations only";

/// The decision for `op` by `caller` under the Tenant's `table` at `now`.
#[must_use]
pub fn decide(
    table: Option<&Table>,
    caller: &Caller,
    op: &Operation<'_>,
    now: DateTime<Utc>,
) -> Decision {
    let platform = match caller {
        Caller::Internal => {
            return if ["5.9.", "5.10.", "5.11."]
                .iter()
                .any(|p| op.clause.starts_with(p))
            {
                Decision::Allow
            } else {
                Decision::Deny(INTERNAL_ONLY.into())
            }
        }
        Caller::Platform(p) => p,
    };
    let Some(table) = table else {
        return Decision::Deny(NOT_GRANTED.into());
    };
    let applying: Vec<&Policy> = held(table, platform, now)
        .filter(|p| p.clauses.contains(op.clause))
        .collect();
    let types = entity_types(op);
    // GW6: a refusal names no policy, so a caller cannot map the rules
    if applying
        .iter()
        .any(|p| p.prohibit && covers_types(p, &types))
    {
        return Decision::Deny(PROHIBITED.into());
    }
    let grants: Vec<&Policy> = applying.into_iter().filter(|p| !p.prohibit).collect();
    if grants.is_empty() {
        return Decision::Deny(NOT_GRANTED.into());
    }
    if FILTERABLE.contains(&op.clause) {
        read(&grants, op, &types, platform)
    } else if WHOLE_TENANT.contains(&op.clause) {
        if grants.iter().any(|p| unconditional_whole(p)) {
            Decision::Allow
        } else {
            Decision::Deny(NOT_GRANTED.into())
        }
    } else if op.clause.starts_with("5.6.") {
        write(&grants, op, &types)
    } else {
        Decision::Allow
    }
}

/// The policies of `table` whose assignee is `caller` and that are in force
/// at `now`: what a decision starts from, and all the access surface shows.
pub fn held<'t>(
    table: &'t Table,
    caller: &'t Platform,
    now: DateTime<Utc>,
) -> impl Iterator<Item = &'t Policy> {
    table
        .policies
        .iter()
        .filter(move |p| caller.is(&p.assignee) && p.in_force(now))
}

/// The Entity types an operation names: the request's, and the stored
/// Entity's for a write by id.
fn entity_types(op: &Operation<'_>) -> Vec<String> {
    let mut out: Vec<String> = op.types.to_vec();
    if let Some(stored) = op.stored {
        out.extend(stored.types.iter().cloned());
    }
    out.sort();
    out.dedup();
    out
}

/// A prohibition covers an operation that names no type, or a type it
/// names, or every operation when it names none (platform GW4).
#[must_use]
pub fn covers_types(p: &Policy, types: &[String]) -> bool {
    p.selectors.is_empty()
        || types.is_empty()
        || p.selectors
            .iter()
            .any(|s| s.types.iter().any(|t| types.contains(t)))
}

fn unconditional_whole(p: &Policy) -> bool {
    p.selectors.is_empty() && !p.is_conditional()
}

/// A read: one rule per selector of every grant (ADR-0023 rule 11).
fn read(grants: &[&Policy], op: &Operation<'_>, types: &[String], caller: &Platform) -> Decision {
    if grants.iter().any(|p| unconditional_whole(p)) {
        return Decision::Allow;
    }
    let mut rules: Vec<Rule> = Vec::new();
    for p in grants {
        if p.selectors.is_empty() {
            // every type, under a condition: a rule per type the request names
            if types.is_empty() {
                return Decision::Deny(NAME_THE_TYPES.into());
            }
            rules.extend(types.iter().map(|t| Rule {
                types: vec![t.clone()],
                ..p.conditions.clone()
            }));
        } else {
            rules.extend(p.selectors.iter().cloned());
        }
    }
    Decision::Filter(Filter {
        warning: left_out(&rules, op, types),
        rules,
        restricted: caller.restricted,
        ..Filter::default()
    })
}

/// The advisory naming the types the request's filter leaves out (map K18, MP-02), as the
/// gateway's oracle sent it: a type whose rules do not show an Attribute `q` names, or, without
/// a `q`, none of the Attributes `attrs` names, answers nothing, and the caller would read that
/// as no data. Only types the grants show (and the request names, when it names any) are
/// named, so the text tells nothing the caller may not read. An `orderBy` leaves no Entity out
/// and is not counted.
fn left_out(rules: &[Rule], op: &Operation<'_>, types: &[String]) -> Option<Warning> {
    let ctx = context();
    // The handler hands `q` over as written; its names expand as the grants' own did.
    let referenced: Vec<String> =
        op.q.map(|q| q.attribute_paths())
            .unwrap_or_default()
            .into_iter()
            .filter(|name| antares_ql::is_non_system_attr(name))
            .map(|name| ctx.expand_key(name))
            .collect();
    let asked: Vec<&str> = match op.q {
        Some(_) => Vec::new(),
        None => op.attrs.iter().map(String::as_str).collect(),
    };
    if referenced.is_empty() && asked.is_empty() {
        return None;
    }
    // What each type shows: `None` is every Attribute.
    let mut shown: BTreeMap<&str, Option<BTreeSet<&str>>> = BTreeMap::new();
    for rule in rules {
        for t in rule
            .types
            .iter()
            .filter(|t| types.is_empty() || types.contains(t))
        {
            let entry = shown.entry(t).or_insert_with(|| Some(BTreeSet::new()));
            match (entry.as_mut(), rule.every_attribute) {
                (Some(held), false) => held.extend(rule.attributes.iter().map(String::as_str)),
                _ => *entry = None,
            }
        }
    }
    let names: Vec<String> = shown
        .into_iter()
        .filter_map(|(t, attributes)| {
            let attributes = attributes?;
            let unserved = referenced.iter().any(|a| !attributes.contains(a.as_str()))
                || (!asked.is_empty() && !asked.iter().any(|a| attributes.contains(a)));
            unserved.then(|| ctx.compact_iri(t))
        })
        .collect();
    warning(&names)
}

/// The gateway's text for `names`, as many of them as one header line holds.
fn warning(names: &[String]) -> Option<Warning> {
    let verb = if names.len() == 1 { "was" } else { "were" };
    let tail = |shown: usize| match names.len().saturating_sub(shown) {
        0 => format!(
            " {verb} not queried: this request selects or orders on an attribute they do not \
             serve on this endpoint"
        ),
        more => format!(
            " and {more} more {verb} not queried: this request selects or orders on an \
             attribute they do not serve on this endpoint"
        ),
    };
    let mut listed = String::new();
    let mut text = None;
    for (at, name) in names.iter().enumerate() {
        if !listed.is_empty() {
            listed.push_str(", ");
        }
        listed.push_str(name);
        let line = format!("{listed}{}", tail(at.saturating_add(1)));
        if line.len() > Warning::MAX_LEN {
            break;
        }
        text = Some(line);
    }
    Warning::new(&text?).ok()
}

/// How one grant admits a write.
enum Admit {
    Whole,
    If(Rule),
}

/// A write: whole inside one grant, or refused whole (platform GW16 to
/// GW19); a grant with a condition on the stored Entity answers `AllowIf`
/// on a write by id.
fn write(grants: &[&Policy], op: &Operation<'_>, types: &[String]) -> Decision {
    if let Some(name) = smuggled(op) {
        return Decision::Deny(format!(
            "{name:?} is access control and does not belong in an Entity"
        ));
    }
    let mut conditional = Vec::new();
    for p in grants {
        for admit in admits(p, op, types) {
            match admit {
                Admit::Whole => return Decision::Allow,
                Admit::If(rule) => conditional.push(rule),
            }
        }
    }
    if conditional.is_empty() {
        Decision::Deny(NOT_GRANTED.into())
    } else {
        Decision::AllowIf(Filter {
            rules: conditional,
            ..Filter::default()
        })
    }
}

/// Every way `p` admits the write: one per selector it falls inside.
fn admits(p: &Policy, op: &Operation<'_>, types: &[String]) -> Vec<Admit> {
    // a window is about reading history; it admits no write
    if !p.conditions.temporal.is_empty() {
        return Vec::new();
    }
    let every_type: Vec<Rule>;
    let selectors = if p.selectors.is_empty() {
        every_type = types
            .iter()
            .map(|t| Rule {
                types: vec![t.clone()],
                ..p.conditions.clone()
            })
            .collect();
        &every_type
    } else {
        &p.selectors
    };
    let ctx = context();
    selectors
        .iter()
        .filter(|s| s.types.iter().any(|t| types.contains(t)))
        .filter(|s| op.ids.iter().all(|id| s.selects_id(id)))
        .filter(|s| s.attributes.is_empty() || op.attrs.iter().all(|a| s.attributes.contains(a)))
        .filter(|s| inside_scope(s, op.body))
        .filter(|s| inside_area(s, op.body))
        .filter_map(|s| {
            if !p.is_conditional() {
                Some(Admit::Whole)
            } else if CREATES.contains(&op.clause) {
                op.body
                    .is_some_and(|b| s.holds(b, ctx))
                    .then_some(Admit::Whole)
            } else if WRITE_BY_ID.contains(&op.clause) {
                Some(Admit::If(Rule {
                    attributes: Vec::new(),
                    every_attribute: false,
                    temporal: Vec::new(),
                    ..s.clone()
                }))
            } else {
                None
            }
        })
        .collect()
}

/// The body's `scope` lies inside the policy's scope tree (R29, R30).
fn inside_scope(s: &Rule, body: Option<&Value>) -> bool {
    let Some(granted) = s.scope_q.as_deref() else {
        return true;
    };
    let scopes = match body.and_then(|b| b.get("scope")) {
        None => return true,
        Some(Value::String(one)) => vec![one.as_str()],
        Some(Value::Array(many)) => many.iter().filter_map(Value::as_str).collect(),
        Some(_) => return false,
    };
    scopes
        .iter()
        .all(|scope| scope_paths(granted).any(|path| covers(path, scope)))
}

/// The body's own location lies inside the policy's area (GW16); a body
/// that carries none is not placed outside it.
fn inside_area(s: &Rule, body: Option<&Value>) -> bool {
    let ctx = context();
    s.areas().all(|area| {
        let iri = if area.geoproperty.is_empty() {
            antares_ql::geo::LOCATION_IRI.to_owned()
        } else {
            ctx.expand_key(&area.geoproperty)
        };
        body.is_none_or(|b| b.get(&iri).is_none() || area.matches(b, ctx))
    })
}

/// A member of [`POLICY_VOCABULARY`] the write carries, if any.
fn smuggled(op: &Operation<'_>) -> Option<&'static str> {
    let ctx = context();
    POLICY_VOCABULARY.into_iter().find(|name| {
        let iri = ctx.expand_key(name);
        op.attrs.iter().any(|a| a == name || *a == iri)
            || op
                .body
                .and_then(Value::as_object)
                .is_some_and(|o| o.contains_key(*name) || o.contains_key(&iri))
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::document::compile;
    use serde_json::json;

    const AREA: &str =
        "georel=within;geometry=Polygon;coordinates=[[[0,0],[1,0],[1,1],[0,1],[0,0]]]";

    fn table(policy: &Value) -> Result<Table, String> {
        let mut p = json!({"id": "p", "assignee": {"kind": "role", "id": "r"},
                           "operations": ["5.6.1", "5.6.8", "5.8.1"],
                           "information": [{"entities": [{"type": "Car"}]}]});
        if let (Some(o), Some(extra)) = (p.as_object_mut(), policy.as_object()) {
            o.extend(extra.clone());
        }
        compile(&json!({ "policies": [p] }))
    }

    fn caller() -> Caller {
        Caller::Platform(Platform {
            roles: ["r".to_owned()].into(),
            ..Platform::default()
        })
    }

    fn car() -> Vec<String> {
        vec![context().expand_key("Car")]
    }

    fn on(table: &Table, clause: &'static str, body: Option<&Value>) -> Decision {
        let types = car();
        let op = Operation {
            types: &types,
            body,
            ..Operation::new(clause)
        };
        decide(Some(table), &caller(), &op, Utc::now())
    }

    fn iri(name: &str) -> String {
        context().expand_key(name)
    }

    /// `on`, with the request's types, Attributes and the caller's
    /// restriction given.
    fn with(
        table: &Table,
        clause: &'static str,
        types: &[String],
        attrs: &[String],
        body: Option<&Value>,
        restricted: bool,
    ) -> Decision {
        let op = Operation {
            types,
            attrs,
            body,
            ..Operation::new(clause)
        };
        let caller = Caller::Platform(Platform {
            roles: ["r".to_owned()].into(),
            restricted,
            ..Platform::default()
        });
        decide(Some(table), &caller, &op, Utc::now())
    }

    /// T-3729: a prohibition naming no type covers a typed operation, and
    /// one naming types covers an operation that names none.
    #[test]
    fn a_prohibition_covers_what_its_types_do_not_tell_apart() -> Result<(), String> {
        let prohibited = Decision::Deny(PROHIBITED.into());
        let every_type = table(&json!({"effect": "prohibit", "information": [],
                                       "operations": ["5.7.2"]}))?;
        assert_eq!(
            with(&every_type, "5.7.2", &car(), &[], None, false),
            prohibited
        );
        let cars = table(&json!({"effect": "prohibit", "operations": ["5.7.2"]}))?;
        assert_eq!(with(&cars, "5.7.2", &[], &[], None, false), prohibited);
        assert_eq!(
            with(&cars, "5.7.2", &[iri("Bus")], &[], None, false),
            Decision::Deny(NOT_GRANTED.into()),
            "another type is not prohibited, nor granted"
        );
        Ok(())
    }

    /// T-3729: a narrowed read carries the caller's wish to be told.
    #[test]
    fn a_read_carries_the_callers_restriction() -> Result<(), String> {
        let t = table(&json!({"operations": ["5.7.2"]}))?;
        for restricted in [true, false] {
            let Decision::Filter(f) = with(&t, "5.7.2", &car(), &[], None, restricted) else {
                return Err("a read by selector is a filter".into());
            };
            assert_eq!(f.restricted, restricted);
        }
        Ok(())
    }

    /// T-3729: the condition a write by id is held to is the selector's
    /// condition alone, never what it shows or a window of its history.
    #[test]
    fn a_held_write_carries_the_condition_alone() -> Result<(), String> {
        let speed = [iri("speed")];
        let mut t = table(&json!({"q": "speed>1", "operations": ["5.6.3"],
            "information": [{"entities": [{"type": "Car"}], "propertyNames": ["speed"]}]}))?;
        let whole = table(&json!({"q": "speed>1", "operations": ["5.6.3"]}))?;
        let window = table(&json!({"temporalQ": "timerel=after;timeAt=P-1D"}))?
            .policies
            .first()
            .map(|p| p.conditions.temporal.clone())
            .filter(|w| !w.is_empty())
            .ok_or("no window")?;
        // a selector given a window of its own still admits no history
        if let Some(s) = t.policies.first_mut().and_then(|p| p.selectors.first_mut()) {
            s.temporal = window;
        }
        for t in [&t, &whole] {
            let Decision::AllowIf(held) = with(t, "5.6.3", &car(), &speed, None, false) else {
                return Err("a conditioned write by id is held".into());
            };
            let rule = held.rules.first().ok_or("no rule")?;
            assert!(rule.q.is_some() && rule.types == car());
            assert!(
                rule.attributes.is_empty() && !rule.every_attribute && rule.temporal.is_empty()
            );
        }
        Ok(())
    }

    /// T-3729: a write by id whose body moves the Entity out of the granted
    /// scope is refused before it is held to any condition.
    #[test]
    fn a_write_by_id_into_another_scope_is_refused() -> Result<(), String> {
        let t = table(&json!({"scopeQ": "/city", "operations": ["5.6.3"]}))?;
        let body = |s: &str| json!({ "scope": s });
        assert!(matches!(
            with(&t, "5.6.3", &car(), &[], Some(&body("/city/a")), false),
            Decision::AllowIf(_)
        ));
        assert_eq!(
            with(&t, "5.6.3", &car(), &[], Some(&body("/town")), false),
            Decision::Deny(NOT_GRANTED.into())
        );
        Ok(())
    }

    /// T-3729: access control written into the data is refused in each
    /// place it can hide: an Attribute named short or expanded, a body
    /// member short or expanded.
    #[test]
    fn access_control_in_the_data_is_refused_in_every_form() -> Result<(), String> {
        let t = table(&json!({}))?;
        let plain = json!({ "type": car() });
        assert_eq!(
            with(&t, "5.6.1", &car(), &[], Some(&plain), false),
            Decision::Allow
        );
        let member = |k: String| json!({ "type": car(), (k): 1 });
        for (attrs, body) in [
            (vec!["owner".to_owned()], plain.clone()),
            (vec![iri("owner")], plain.clone()),
            (Vec::new(), member("acl".into())),
            (Vec::new(), member(iri("acl"))),
        ] {
            let d = with(&t, "5.6.1", &car(), &attrs, Some(&body), false);
            assert!(
                matches!(&d, Decision::Deny(why) if why.contains("is access control")),
                "{attrs:?} {body}: {d:?}"
            );
        }
        Ok(())
    }

    #[test]
    fn a_granted_clause_outside_the_entity_operations_is_allowed() -> Result<(), String> {
        let t = table(&json!({"q": "speed>1"}))?;
        assert_eq!(on(&t, "5.8.1", None), Decision::Allow);
        Ok(())
    }

    #[test]
    fn a_conditional_grant_holds_a_batch_upsert_by_id_and_a_window_no_write() -> Result<(), String>
    {
        // T-3668: an upserted member is judged on the stored Entity inside
        // the store write, and a conditioned upsert never creates
        let t = table(&json!({"q": "speed>1"}))?;
        let Decision::AllowIf(held) = on(&t, "5.6.8", None) else {
            return Err("a conditioned upsert is held to the condition".into());
        };
        assert_eq!(held.rules.len(), 1);
        assert!(held
            .rules
            .first()
            .is_some_and(|r| r.q.is_some() && r.types == car()));
        let t = table(&json!({"temporalQ": "timerel=after;timeAt=P-1D"}))?;
        assert_eq!(on(&t, "5.6.1", None), Decision::Deny(NOT_GRANTED.into()));
        Ok(())
    }

    #[test]
    fn a_body_scope_is_judged_in_each_shape() -> Result<(), String> {
        let t = table(&json!({"scopeQ": "/city"}))?;
        let scope = |s: Value| json!({ "type": car(), "scope": s });
        for (body, granted) in [
            // a create outside every scope does not hold the condition
            (json!({ "type": car() }), false),
            (scope(json!("/city/north")), true),
            (scope(json!(["/city", "/city/a"])), true),
            (scope(json!("/town")), false),
            (scope(json!(7)), false),
        ] {
            let want = if granted {
                Decision::Allow
            } else {
                Decision::Deny(NOT_GRANTED.into())
            };
            assert_eq!(on(&t, "5.6.1", Some(&body)), want, "{body}");
        }
        Ok(())
    }

    #[test]
    fn an_area_on_a_named_geoproperty_reads_that_member() -> Result<(), String> {
        let t = table(&json!({"geoQ": format!("{AREA};geoproperty=parking")}))?;
        let at = |x: f64| {
            json!({ "type": car(), context().expand_key("parking"): [{"type": "GeoProperty",
                "value": {"type": "Point", "coordinates": [x, 0.5]}}] })
        };
        assert_eq!(on(&t, "5.6.1", Some(&at(0.5))), Decision::Allow);
        assert_eq!(
            on(&t, "5.6.1", Some(&at(5.0))),
            Decision::Deny(NOT_GRANTED.into())
        );
        Ok(())
    }

    /// The warning a read by `q` (or `attrs` alone) carries, over a grant
    /// showing `speed` of a Car and `name` of a Bus.
    fn left_out_by(
        types: &[&str],
        q: Option<&str>,
        attrs: &[&str],
    ) -> Result<Option<String>, String> {
        let t = table(&json!({"operations": ["5.7.2"], "information": [
            {"entities": [{"type": "Car"}], "propertyNames": ["speed"]},
            {"entities": [{"type": "Bus"}], "propertyNames": ["name"]}]}))?;
        let types: Vec<String> = types.iter().map(|n| iri(n)).collect();
        let attrs: Vec<String> = attrs.iter().map(|n| iri(n)).collect();
        let q = q
            .map(antares_ql::parse_q)
            .transpose()
            .map_err(|e| e.to_string())?;
        let op = Operation {
            types: &types,
            attrs: &attrs,
            q: q.as_ref(),
            ..Operation::new("5.7.2")
        };
        let Decision::Filter(f) = decide(Some(&t), &caller(), &op, Utc::now()) else {
            return Err("a read by selector is a filter".into());
        };
        Ok(f.warning.map(|w| w.texts().join("; ")))
    }

    const WAS: &str = "was not queried: this request selects or orders on an attribute they do \
                       not serve on this endpoint";
    const WERE: &str = "were not queried: this request selects or orders on an attribute they \
                        do not serve on this endpoint";

    /// T-3796 (MP-02): a filter on an attribute a type does not show names
    /// that type, as the gateway did.
    #[test]
    fn a_filter_names_the_one_type_it_left_out() -> Result<(), String> {
        assert_eq!(
            left_out_by(&["Car", "Bus"], Some("speed>1"), &[])?,
            Some(format!("Bus {WAS}"))
        );
        assert_eq!(
            left_out_by(&["Car", "Bus"], None, &["name"])?,
            Some(format!("Car {WAS}")),
            "attrs alone leaves out the type that shows none of them"
        );
        Ok(())
    }

    /// T-3796: two types left out are both named; none left out is no warning.
    #[test]
    fn a_filter_names_two_types_or_none() -> Result<(), String> {
        assert_eq!(
            left_out_by(&[], Some("colour==\"red\""), &[])?,
            Some(format!("Bus, Car {WERE}"))
        );
        assert_eq!(left_out_by(&["Car"], Some("speed>1"), &[])?, None);
        assert_eq!(
            left_out_by(&["Car", "Bus"], None, &["speed", "name"])?,
            None
        );
        assert_eq!(left_out_by(&["Car", "Bus"], None, &[])?, None);
        assert_eq!(
            left_out_by(&["Car"], Some("createdAt>\"2020-01-01T00:00:00Z\""), &[])?,
            None,
            "a system member is never left out"
        );
        Ok(())
    }

    /// T-3796: a type the subject may not read is never named, and a type
    /// shown whole leaves nothing out.
    #[test]
    fn a_type_the_subject_may_not_read_is_never_named() -> Result<(), String> {
        assert_eq!(
            left_out_by(&["Car", "Secret"], Some("name==\"x\""), &[])?,
            Some(format!("Car {WAS}"))
        );
        let whole = table(&json!({"operations": ["5.7.2"]}))?;
        let q = antares_ql::parse_q("anything>1").map_err(|e| e.to_string())?;
        let op = Operation {
            types: &car(),
            q: Some(&q),
            ..Operation::new("5.7.2")
        };
        let Decision::Filter(f) = decide(Some(&whole), &caller(), &op, Utc::now()) else {
            return Err("a read by selector is a filter".into());
        };
        assert_eq!(f.warning, None);
        Ok(())
    }

    /// T-3796: the names one header line cannot hold are counted, not cut.
    #[test]
    fn the_warning_fits_one_header_line() -> Result<(), String> {
        let names: Vec<String> = (0..40)
            .map(|i| format!("AVeryLongTypeName{i:02}"))
            .collect();
        let w = warning(&names).ok_or("no warning")?;
        let text = w.texts().join("");
        assert!(text.len() <= Warning::MAX_LEN, "{text}");
        assert!(
            text.starts_with("AVeryLongTypeName00, AVeryLongTypeName01"),
            "{text}"
        );
        assert!(text.contains(" more were not queried"), "{text}");
        assert_eq!(warning(&[]), None);
        assert_eq!(
            warning(&["x".repeat(300)]),
            None,
            "a name past the bound is not cut"
        );
        Ok(())
    }
}
