// SPDX-License-Identifier: EUPL-1.2
//! How a type records history (T-4665, platform ADR-N-054 2.2): `every`
//! writes a row per new `observedAt`, even with the same value; `onChange`
//! writes one when the value moves past its deadband or minimum distance,
//! when a value of the same instant is corrected, and once per heartbeat.

use antares_api::{AppState, OnChange, Recording};
use antares_sql::store::any::AnyStore;
use antares_sql::store::Store;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use http_body_util::BodyExt;
use serde_json::{json, Value};
use std::sync::Arc;
use std::time::Duration;
use tower::ServiceExt;

const CORE: &str = "https://uri.etsi.org/ngsi-ld/default-context/";

async fn state(recording: Option<Recording>) -> AppState {
    let store = Arc::new(AnyStore::Mem(Store::default()));
    let mut st = AppState::with_drivers("me".into(), store.clone(), store, "memory");
    antares_api::wire(&mut st).await;
    if let Some(recording) = recording {
        st.history_off.write().expect("rules").recording.insert(
            (
                antares_model::TenantId::default().as_str().to_owned(),
                format!("{CORE}Spot"),
            ),
            recording,
        );
    }
    st
}

async fn req(st: &AppState, method: &str, path: &str, body: Option<Value>) -> (StatusCode, Value) {
    let mut b = Request::builder().method(method).uri(path);
    let body = match body {
        Some(v) => {
            let s = v.to_string();
            b = b
                .header("Content-Type", "application/json")
                .header("Content-Length", s.len());
            Body::from(s)
        }
        None => Body::empty(),
    };
    let resp = antares_api::router(st.clone())
        .oneshot(b.body(body).expect("req"))
        .await
        .expect("resp");
    let status = resp.status();
    let bytes = resp.into_body().collect().await.expect("body").to_bytes();
    (
        status,
        serde_json::from_slice(&bytes).unwrap_or(Value::Null),
    )
}

/// Writes `writes` (value, minute) of one attribute to a fresh Spot and
/// answers the values its history recorded.
async fn recorded(st: &AppState, id: &str, attr: &str, writes: &[(Value, u32)]) -> Vec<Value> {
    let at = |m: u32| format!("2026-10-18T{:02}:{:02}:00Z", 7 + m / 60, m % 60);
    let (first, m0) = &writes[0];
    let (s, b) = req(
        st,
        "POST",
        "/ngsi-ld/v1/entities",
        Some(json!({"id": id, "type": "Spot",
                    attr: {"type": "Property", "value": first, "observedAt": at(*m0)}})),
    )
    .await;
    assert_eq!(s, 201, "{b}");
    for (value, m) in &writes[1..] {
        let (s, b) = req(
            st,
            "PATCH",
            &format!("/ngsi-ld/v1/entities/{id}/attrs"),
            Some(json!({attr: {"type": "Property", "value": value, "observedAt": at(*m)}})),
        )
        .await;
        assert_eq!(s, 204, "{b}");
    }
    let (s, doc) = req(
        st,
        "GET",
        &format!("/ngsi-ld/v1/temporal/entities/{id}"),
        None,
    )
    .await;
    assert_eq!(s, 200, "{doc}");
    let rows = match doc.get(attr) {
        Some(Value::Array(a)) => a.clone(),
        Some(v) => vec![v.clone()],
        None => vec![],
    };
    rows.iter().map(|r| r["value"].clone()).collect()
}

fn on_change(heartbeat_minutes: u64, deadband: Option<f64>) -> Option<OnChange> {
    Some(OnChange {
        heartbeat: Duration::from_secs(heartbeat_minutes * 60),
        deadband,
        min_distance: None,
    })
}

#[tokio::test(flavor = "multi_thread")]
async fn every_records_the_same_value_at_each_new_instant() {
    let st = state(None).await;
    let writes = [(json!("free"), 0), (json!("free"), 1), (json!("free"), 2)];
    assert_eq!(
        recorded(&st, "urn:ngsi-ld:Spot:every", "status", &writes)
            .await
            .len(),
        3
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn on_change_records_a_change_and_a_heartbeat_and_nothing_in_between() {
    let st = state(Some(Recording {
        default: on_change(60, None),
        ..Default::default()
    }))
    .await;
    let writes = [
        (json!("free"), 0),
        (json!("free"), 1),      // unchanged: no row
        (json!("occupied"), 2),  // changed: a row
        (json!("occupied"), 30), // unchanged inside the heartbeat: no row
        (json!("occupied"), 62), // the heartbeat passed: a row
    ];
    let rows = recorded(&st, "urn:ngsi-ld:Spot:status", "status", &writes).await;
    assert_eq!(rows, [json!("free"), json!("occupied"), json!("occupied")]);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_deadband_holds_back_small_moves_and_an_attribute_may_record_every() {
    let recording = Recording {
        default: on_change(60, Some(1.0)),
        attributes: [(format!("{CORE}count"), None)].into_iter().collect(),
    };
    let st = state(Some(recording)).await;
    let level = [
        (json!(10.0), 0),
        (json!(10.6), 1),
        (json!(11.2), 2),
        (json!(10.9), 3),
    ];
    // 10.6 and 10.9 stay within 1.0 of the last row; 11.2 is 1.2 past 10.0.
    assert_eq!(
        recorded(&st, "urn:ngsi-ld:Spot:level", "occupancy", &level).await,
        [json!(10.0), json!(11.2)]
    );
    // A count per interval is never left out (its own `every`).
    let count = [(json!(3), 0), (json!(3), 1), (json!(3), 2)];
    assert_eq!(
        recorded(&st, "urn:ngsi-ld:Spot:count", "count", &count)
            .await
            .len(),
        3
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_correction_of_the_same_instant_is_recorded_whatever_the_deadband() {
    let st = state(Some(Recording {
        default: on_change(60, Some(5.0)),
        ..Default::default()
    }))
    .await;
    let writes = [(json!(10.0), 0), (json!(10.5), 0)];
    let rows = recorded(&st, "urn:ngsi-ld:Spot:fix", "occupancy", &writes).await;
    assert_eq!(
        rows,
        [json!(10.5)],
        "the instant's row now holds the corrected value"
    );
}
