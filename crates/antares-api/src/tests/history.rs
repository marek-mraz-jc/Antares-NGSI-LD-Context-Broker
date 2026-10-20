// SPDX-License-Identifier: EUPL-1.2
//! The temporal producer's gates (T-4275, T-4665): an Entity's own
//! retention, the Tenant's type rules, and `onChange` recording with its
//! deadband, minimum distance and heartbeat.

use crate::history::{
    admits_change, held_attrs, own_retention, parse_duration, records_history, records_on_change,
    retention_of, OwnRetention,
};
use crate::state::{AppState, OnChange, Recording};
use antares_model::TenantId;
use serde_json::json;
use std::time::Duration;

const LEVEL: &str = "https://x/level";
const PLACE: &str = "https://x/place";

fn state() -> AppState {
    AppState::new("antares-test".into())
}

#[test]
fn an_entity_names_its_own_retention_or_a_reason_it_cannot() {
    let rule = |v: serde_json::Value| json!({ antares_store::HISTORY_RETENTION: v });
    assert_eq!(own_retention(&json!({})), None);
    assert_eq!(
        own_retention(&rule(json!({"type": "Property", "value": "off"}))),
        Some(Ok(OwnRetention::Off))
    );
    assert_eq!(
        own_retention(&rule(json!([{"type": "Property", "value": "P30D"},
                                   {"type": "Property", "value": "off"}]))),
        Some(Ok(OwnRetention::Keep(Duration::from_secs(30 * 86_400)))),
        "the first instance counts"
    );
    let typo = own_retention(&rule(json!({"type": "Property", "value": "forever"})));
    assert!(typo.is_some_and(|r| r.is_err_and(|e| e.contains("forever"))));
    assert!(retention_of(&json!({"type": "Property", "value": 30})).is_err());
    assert!(retention_of(&json!([])).is_err());
}

#[test]
fn a_duration_is_read_in_both_forms_within_its_bounds() {
    for (raw, secs) in [
        ("PT5M", 300),
        ("PT1H30M", 5400),
        ("P1DT12H", 129_600),
        ("PT90S", 90),
        ("90s", 90),
        ("5m", 300),
        ("8h", 28_800),
        ("14d", 1_209_600),
    ] {
        assert_eq!(parse_duration(raw), Ok(Duration::from_secs(secs)), "{raw}");
    }
    for bad in [
        "", "P", "PT", "P1DT", "P1W", "PTxM", "PT5M3", "5", "5w", "xm", "PT5S",
    ] {
        assert!(
            parse_duration(bad).is_err(),
            "{bad:?} is no duration in bounds"
        );
    }
    let past_ten_years = parse_duration("P3651D").expect_err("over ten years");
    assert!(past_ten_years.contains("outside"), "{past_ten_years}");
}

#[test]
fn the_most_specific_retention_rule_decides_what_records() {
    let st = state();
    let t = TenantId::new("history-rules").expect("tenant");
    let doc = |types: serde_json::Value| json!({"id": "urn:ngsi-ld:Room:1", "type": types});
    assert!(
        records_history(&st, &t, &doc(json!("https://x/Room"))),
        "no rule: all"
    );
    {
        let mut rules = st.history_off.write().expect("rules");
        rules
            .off
            .insert(("history-rules".into(), "https://x/Secret".into()));
        rules
            .ruled
            .insert(("history-rules".into(), "https://x/Room".into()));
        rules.space_off.insert("history-rules".into());
    }
    assert!(
        !records_history(&st, &t, &doc(json!(["https://x/Room", "https://x/Secret"]))),
        "any type ruled off records nothing"
    );
    assert!(
        records_history(&st, &t, &doc(json!("https://x/Room"))),
        "a type's own rule beats the Tenant's `*` off"
    );
    assert!(
        !records_history(&st, &t, &doc(json!("https://x/Hall"))),
        "the Tenant's `*` off"
    );
    assert!(
        !records_history(&st, &t, &json!({"id": "urn:ngsi-ld:X:1"})),
        "no type: the `*` rule"
    );
    // the Entity's own retention outranks every rule
    let own = |v: &str| {
        json!({"type": "https://x/Hall",
        antares_store::HISTORY_RETENTION: {"type": "Property", "value": v}})
    };
    assert!(records_history(&st, &t, &own("P7D")));
    assert!(!records_history(&st, &t, &own("off")));
    assert!(
        !records_history(&st, &t, &own("forever")),
        "a typo falls back to the rules"
    );
}

fn recording(st: &AppState, tenant: &str, ty: &str, attrs: &[(&str, OnChange)]) {
    let rec = Recording {
        default: None,
        attributes: attrs
            .iter()
            .map(|(a, m)| ((*a).to_owned(), Some(*m)))
            .collect(),
    };
    st.history_off
        .write()
        .expect("rules")
        .recording
        .insert((tenant.to_owned(), ty.to_owned()), rec);
}

fn level(value: f64, at: &str) -> serde_json::Value {
    json!({"type": "Property", "value": value, "observedAt": at,
           "instanceId": "urn:ngsi-ld:i:1", "modifiedAt": at})
}

#[test]
fn on_change_writes_a_row_past_the_deadband_or_the_heartbeat() {
    let st = state();
    let tn = "history-deadband";
    let t = TenantId::new(tn).expect("tenant");
    let ty = ["https://x/Tank"];
    assert!(!records_on_change(&st, &t));
    assert!(
        admits_change(
            &st,
            &t,
            &ty,
            "urn:e",
            LEVEL,
            &level(1.0, "2026-01-01T00:00:00Z")
        ),
        "no rule anywhere: every write is a row"
    );
    let mode = OnChange {
        heartbeat: Duration::from_secs(3600),
        deadband: Some(0.5),
        min_distance: None,
    };
    recording(&st, tn, ty[0], &[(LEVEL, mode)]);
    assert!(records_on_change(&st, &t));
    assert!(!records_on_change(
        &st,
        &TenantId::new("history-other").expect("tenant")
    ));

    let row = |v, at| admits_change(&st, &t, &ty, "urn:e", LEVEL, &level(v, at));
    assert!(
        row(10.0, "2026-01-01T00:00:00Z"),
        "the first value is a row"
    );
    assert!(!row(10.4, "2026-01-01T00:10:00Z"), "inside the deadband");
    assert!(row(10.6, "2026-01-01T00:20:00Z"), "past the deadband");
    assert!(!row(10.6, "2026-01-01T00:20:00Z"), "the same row again");
    assert!(
        row(10.7, "2026-01-01T00:20:00Z"),
        "a correction of the same instant"
    );
    assert!(
        row(10.0, "2026-01-01T00:05:00Z"),
        "older than the last row: a backfill"
    );
    assert!(row(10.0, "2026-01-01T02:00:00Z"), "the heartbeat passed");
    assert!(
        admits_change(
            &st,
            &t,
            &ty,
            "urn:e",
            "https://x/other",
            &level(1.0, "2026-01-01T02:00:00Z")
        ),
        "an attribute without a rule records every write"
    );
    assert!(
        admits_change(
            &st,
            &t,
            &["https://x/Pump"],
            "urn:e",
            LEVEL,
            &level(1.0, "2026-01-01T02:00:00Z")
        ),
        "a type without a rule records every write"
    );

    let doc = json!({"id": "urn:e", "type": ty[0], LEVEL: level(1.0, "2026-01-01T00:00:00Z"), "https://x/other": {}});
    assert_eq!(
        held_attrs(&st, &t, &doc).into_iter().collect::<Vec<_>>(),
        vec![LEVEL.to_owned()]
    );
    let many = json!({"type": [ty[0], "https://x/Pump"], LEVEL: {}});
    assert_eq!(held_attrs(&st, &t, &many).len(), 1);
    assert!(held_attrs(&st, &t, &json!("not an entity")).is_empty());
    assert!(
        held_attrs(&state(), &t, &doc).is_empty(),
        "no recording rule at all"
    );
}

#[test]
fn on_change_of_a_place_counts_metres_and_of_a_text_any_change() {
    let st = state();
    let tn = "history-distance";
    let t = TenantId::new(tn).expect("tenant");
    let ty = ["https://x/Bike"];
    let mode = OnChange {
        heartbeat: Duration::from_secs(3600),
        deadband: None,
        min_distance: Some(50.0),
    };
    recording(&st, tn, ty[0], &[(PLACE, mode), ("https://x/state", mode)]);
    let at = |lon: f64, lat: f64, when: &str| {
        json!({"type": "GeoProperty", "observedAt": when,
               "value": {"type": "Point", "coordinates": [lon, lat]}})
    };
    let row = |v: serde_json::Value| admits_change(&st, &t, &ty, "urn:b", PLACE, &v);
    assert!(row(at(24.9384, 60.1699, "2026-01-01T00:00:00Z")));
    // 0.0003° of latitude is about 33 m
    assert!(
        !row(at(24.9384, 60.1702, "2026-01-01T00:01:00Z")),
        "within the minimum distance"
    );
    assert!(
        row(at(24.9384, 60.1709, "2026-01-01T00:02:00Z")),
        "about 110 m moved"
    );
    let text = |v: &str, when: &str| {
        admits_change(
            &st,
            &t,
            &ty,
            "urn:b",
            "https://x/state",
            &json!({"type": "Property", "value": v, "observedAt": when}),
        )
    };
    assert!(text("docked", "2026-01-01T00:00:00Z"));
    assert!(!text("docked", "2026-01-01T00:01:00Z"), "no change, no row");
    assert!(
        text("riding", "2026-01-01T00:02:00Z"),
        "a value that is no number or point: any change"
    );
}
