// SPDX-License-Identifier: EUPL-1.2
//! The `{type}-history` collections (T-5100): a Feature per Entity and `observedAt`, its
//! `validFrom`/`validTo`, the location valid then, `datetime`, `bbox`, `filter`, paging, the
//! Feature by id, no write, and the view's rules and Policy narrowing the history.

use crate::common::{fixture_with, get, href, ids, local, schemas_on, send, SlowOnly};
use antares_ops::test_kit::{q_filter, TestBroker};
use axum::http::StatusCode;
use serde_json::{json, Value};
use std::sync::Arc;

const PAST: &str = "/x/views/past/ogc/features";
const ITEMS: &str = "/x/views/past/ogc/features/collections/Car-history/items";
const T0: &str = "2026-10-01T10:00:00Z";
const T1: &str = "2026-10-01T10:05:00Z";
const T2: &str = "2026-10-01T10:10:00Z";

fn at(v: &Value, t: &str) -> Value {
    json!({"type": "Property", "value": v, "observedAt": t})
}

fn moved(lon: f64, lat: f64, t: &str) -> Value {
    json!({"type": "GeoProperty", "value": {"type": "Point", "coordinates": [lon, lat]}, "observedAt": t})
}

/// A broker that takes schemas: the bound is read when it starts.
async fn fresh() -> Result<TestBroker, String> {
    schemas_on();
    TestBroker::new().await
}

/// The fixture, a view `past` that offers the Temporal Query beside Query Entities, and the
/// histories of Car 1 (speed 10 at T0 and 20 at T2, moved at T1, a hidden `secret` at T0)
/// Car 2 (speed 60 and its place at T1) and Car 4 (speed 70 at T0, its place never observed).
async fn past(broker: TestBroker) -> Result<crate::common::Fixture, String> {
    let f = fixture_with(broker).await?;
    let view = json!({
        "id": "past", "tenant": "city", "selector": ["Car", "Stop"],
        "rules": [{"types": ["Car"], "attributes": ["speed", "location", "seen", "at", "home", "colour", "tags"]},
                  {"types": ["Stop"], "attributes": ["name"]}],
        "operations": ["query_entities", "temporal_query"]
    });
    let a = send(&f.app, "PUT", "/q/views/past", &view.to_string(), &[]).await;
    if a.status != StatusCode::CREATED {
        return Err(format!("view past: {} {}", a.status, a.body));
    }
    let histories = [
        json!({"id": "urn:ngsi-ld:Car:1", "type": "Car",
               "speed": [at(&json!(10), T0), at(&json!(20), T2)],
               "location": [moved(17.1, 48.1, T0), moved(17.2, 48.2, T1)],
               "secret": [at(&json!("hidden-in-history"), T0)]}),
        json!({"id": "urn:ngsi-ld:Car:2", "type": "Car",
               "speed": [at(&json!(60), T1)], "location": [moved(179.5, 0.0, T1)]}),
        json!({"id": "urn:ngsi-ld:Car:4", "type": "Car", "speed": [at(&json!(70), T0)]}),
    ];
    for h in histories {
        let a = send(
            &f.app,
            "POST",
            "/ngsi-ld/v1/temporal/entities",
            &h.to_string(),
            &[("NGSILD-Tenant", "city")],
        )
        .await;
        if !a.status.is_success() {
            return Err(format!("{h}: {} {}", a.status, a.body));
        }
    }
    Ok(f)
}

/// Each Feature's speed, longitude and `validTo`, by id.
fn states(body: &Value) -> Vec<(String, Value, Value, Value)> {
    body.get("features")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .map(|f| {
            (
                f.get("id")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_owned(),
                f.pointer("/properties/speed").cloned().unwrap_or_default(),
                f.pointer("/geometry/coordinates/0")
                    .cloned()
                    .unwrap_or_default(),
                f.pointer("/properties/validTo")
                    .cloned()
                    .unwrap_or_default(),
            )
        })
        .collect()
}

fn car(n: u8, t: &str) -> String {
    format!("urn:ngsi-ld:Car:{n}@{t}")
}

#[tokio::test(flavor = "multi_thread")]
async fn a_history_holds_a_feature_per_observed_at_with_its_interval() -> Result<(), String> {
    let f = past(fresh().await?).await?;
    let list = get(&f.app, &format!("{PAST}/collections")).await;
    let names: Vec<&str> = list
        .body
        .get("collections")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|c| c.get("id").and_then(Value::as_str))
        .collect();
    assert_eq!(names, ["Car", "Car-history", "Stop", "Stop-history"]);
    let city = get(&f.app, "/x/views/city/ogc/features/collections/Car-history").await;
    assert_eq!(
        city.status,
        StatusCode::NOT_FOUND,
        "no history where the view offers no Temporal Query"
    );
    let a = get(&f.app, ITEMS).await;
    assert_eq!(a.status, StatusCode::OK, "{}", a.body);
    assert_eq!(
        states(&a.body),
        [
            (car(1, T0), json!(10), json!(17.1), json!(T1)),
            (car(1, T1), json!(10), json!(17.2), json!(T2)),
            (car(1, T2), json!(20), json!(17.2), Value::Null),
            (car(2, T1), json!(60), json!(179.5), Value::Null),
            (car(4, T0), json!(70), json!(-179.5), Value::Null),
        ],
        "the location valid then, else the current one; validTo the next observedAt, open for the newest"
    );
    let first = a.body.pointer("/features/0/properties").cloned();
    assert_eq!(
        first
            .as_ref()
            .and_then(|p| p.get("entityId"))
            .and_then(Value::as_str),
        Some("urn:ngsi-ld:Car:1")
    );
    assert_eq!(
        first
            .as_ref()
            .and_then(|p| p.get("validFrom"))
            .and_then(Value::as_str),
        Some(T0)
    );
    let text = a.body.to_string();
    assert!(
        !text.contains("secret") && !text.contains("hidden-in-history"),
        "an Attribute the view hides is absent from its history"
    );
    let one = get(&f.app, &format!("{ITEMS}/{}", car(1, T1))).await;
    assert_eq!(one.status, StatusCode::OK, "{}", one.body);
    assert_eq!(one.body.pointer("/properties/validTo"), Some(&json!(T2)));
    let none = get(
        &f.app,
        &format!("{ITEMS}/{}", car(1, "2026-10-01T10:01:00Z")),
    )
    .await;
    assert_eq!(none.status, StatusCode::NOT_FOUND);
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn datetime_bbox_and_filter_select_the_states_valid_then() -> Result<(), String> {
    let f = past(fresh().await?).await?;
    let picked = |q: &'static str| {
        let app = f.app.clone();
        async move { ids(&get(&app, &format!("{ITEMS}?{q}")).await.body) }
    };
    assert_eq!(
        picked("datetime=2026-10-01T10:07:00Z").await,
        [car(1, T1), car(2, T1), car(4, T0)],
        "an instant keeps the state valid at it"
    );
    assert_eq!(
        picked("datetime=2026-10-01T10:06:00Z/..").await,
        [car(1, T1), car(1, T2), car(2, T1), car(4, T0)],
        "an open end keeps the newest"
    );
    assert_eq!(
        picked("datetime=../2026-10-01T10:04:00Z").await,
        [car(1, T0), car(4, T0)]
    );
    assert_eq!(
        picked("bbox=17.15,48.15,17.3,48.3").await,
        [car(1, T1), car(1, T2)]
    );
    assert_eq!(
        picked("filter=speed%3E15").await,
        [car(1, T2), car(2, T1), car(4, T0)],
        "CQL2 over the values valid then"
    );
    assert_eq!(
        picked("filter=validTo%20IS%20NULL").await,
        [car(1, T2), car(2, T1), car(4, T0)]
    );
    let q = get(
        &f.app,
        &format!("{PAST}/collections/Car-history/queryables"),
    )
    .await;
    assert_eq!(
        q.body.pointer("/properties/validFrom/x-ogc-role"),
        Some(&json!("primary-interval-start"))
    );
    assert_eq!(
        q.body.pointer("/properties/validTo/x-ogc-role"),
        Some(&json!("primary-interval-end"))
    );
    let sorted = get(&f.app, &format!("{ITEMS}?sortby=speed")).await;
    assert_eq!(
        sorted.status,
        StatusCode::BAD_REQUEST,
        "a history sorts nothing"
    );
    let geo = get(
        &f.app,
        &format!("{ITEMS}?ngsild.georel=near%3BmaxDistance%3D%3D1"),
    )
    .await;
    assert_eq!(geo.status, StatusCode::BAD_REQUEST, "{}", geo.body);
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn a_history_pages_by_next_and_takes_no_write() -> Result<(), String> {
    let f = past(fresh().await?).await?;
    let mut seen = Vec::new();
    let mut at = format!("{ITEMS}?limit=1");
    for _ in 0..8 {
        let a = get(&f.app, &at).await;
        assert_eq!(a.status, StatusCode::OK, "{}", a.body);
        assert!(ids(&a.body).len() <= 1);
        seen.extend(ids(&a.body));
        match href(&a.body, "next") {
            Some(next) => at = local(&next),
            None => break,
        }
    }
    assert_eq!(
        seen,
        [car(1, T0), car(1, T1), car(1, T2), car(2, T1), car(4, T0)]
    );
    let forged = get(&f.app, &format!("{ITEMS}?next=0.0.0000000000000000")).await;
    assert_eq!(forged.status, StatusCode::BAD_REQUEST);
    let body = json!({"type": "Feature", "geometry": null, "properties": {"speed": 1}});
    let post = send(&f.app, "POST", ITEMS, &body.to_string(), &[]).await;
    assert_eq!(post.status, StatusCode::METHOD_NOT_ALLOWED);
    let api = get(&f.app, &format!("{PAST}/api")).await;
    assert!(
        api.body
            .pointer("/paths/~1collections~1Car-history~1items/post")
            .is_none(),
        "the API offers no write on a history"
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn the_policy_narrows_the_history_as_the_current_state() -> Result<(), String> {
    let engine = Arc::new(SlowOnly(q_filter("speed<50")?));
    let f = past(fresh().await?.with_policy(engine)).await?;
    let a = get(&f.app, ITEMS).await;
    assert_eq!(a.status, StatusCode::OK, "{}", a.body);
    assert_eq!(
        ids(&a.body).first().map(String::as_str),
        Some(car(1, T0).as_str()),
        "Car 1, slow now, keeps its history"
    );
    assert!(
        !ids(&a.body)
            .iter()
            .any(|id| id.starts_with("urn:ngsi-ld:Car:2@")),
        "Car 2, fast now, is hidden by the Policy: {:?}",
        ids(&a.body)
    );
    Ok(())
}
