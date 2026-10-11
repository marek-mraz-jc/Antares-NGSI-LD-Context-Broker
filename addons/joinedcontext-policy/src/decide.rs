// SPDX-License-Identifier: EUPL-1.2
//! The decisions of README section 3: prohibitions first, default deny, a
//! read narrowed to one rule per selector, a write whole inside one policy.

use crate::document::{context, covers, scope_paths, Policy, Table};
use crate::subject::{Caller, Platform};
use antares_api::policy::{
    Decision, Filter, Operation, Rule, FILTERABLE, WHOLE_TENANT, WRITE_BY_ID,
};
use chrono::{DateTime, Utc};
use serde_json::Value;

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
    let applying: Vec<&Policy> = table
        .policies
        .iter()
        .filter(|p| platform.is(&p.assignee) && p.clauses.contains(op.clause) && p.in_force(now))
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
        read(&grants, &types, platform)
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
fn covers_types(p: &Policy, types: &[String]) -> bool {
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
fn read(grants: &[&Policy], types: &[String], caller: &Platform) -> Decision {
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
        rules,
        restricted: caller.restricted,
        ..Filter::default()
    })
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

    #[test]
    fn a_granted_clause_outside_the_entity_operations_is_allowed() -> Result<(), String> {
        let t = table(&json!({"q": "speed>1"}))?;
        assert_eq!(on(&t, "5.8.1", None), Decision::Allow);
        Ok(())
    }

    #[test]
    fn a_conditional_grant_admits_no_batch_upsert_and_a_window_no_write() -> Result<(), String> {
        let t = table(&json!({"q": "speed>1"}))?;
        assert_eq!(on(&t, "5.6.8", None), Decision::Deny(NOT_GRANTED.into()));
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
}
