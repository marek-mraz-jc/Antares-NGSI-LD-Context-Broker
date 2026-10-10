// SPDX-License-Identifier: EUPL-1.2
//! What the view loader refuses and what it reports (surfaces-threat-model
//! V16, V27): a view that names nothing to select does not load, and a view
//! that serves history keyed on a member that changes is loaded with a note
//! an operator reads in `/q/health` (ADR-0023 rule 8, the documented
//! residual).

use antares_ops::views::View;
use serde_json::{json, Value};

fn load(doc: &Value) -> Result<antares_ops::views::Loaded, String> {
    serde_json::from_value::<View>(doc.clone())
        .map_err(|e| e.to_string())?
        .load()
}

fn view() -> Value {
    json!({
        "id": "cars", "tenant": "city", "selector": ["Car"],
        "rules": [{"types": ["Car"], "attributes": ["speed"]}],
        "operations": ["query_entities", "temporal_query"]
    })
}

#[test]
fn a_view_without_a_selector_is_refused() {
    let mut empty = view();
    empty["selector"] = json!([]);
    let why = load(&empty).expect_err("an empty selector loads");
    assert!(why.contains("selector"), "{why}");
    let mut absent = view();
    absent.as_object_mut().map(|m| m.remove("selector"));
    assert!(load(&absent).is_err(), "a view without a selector loads");
    // a selector whose types no rule shows selects nothing either
    let mut unshown = view();
    unshown["selector"] = json!(["Truck"]);
    assert!(load(&unshown).is_err(), "a selector no rule shows loads");
    assert!(load(&view()).is_ok());
}

#[test]
fn a_temporal_view_keyed_on_a_mutable_property_is_warned() {
    let rule8 = |l: &antares_ops::views::Loaded| l.notes.iter().any(|n| n.contains("rule 8"));
    let mut keyed = view();
    keyed["q"] = json!("speed<80");
    assert!(
        rule8(&load(&keyed).expect("loads")),
        "a temporal view keyed on speed carries no note"
    );
    // no history served, or no q to key on: nothing to warn about
    let mut current_only = keyed.clone();
    current_only["operations"] = json!(["query_entities"]);
    assert!(!rule8(&load(&current_only).expect("loads")));
    assert!(!rule8(&load(&view()).expect("loads")));
}
