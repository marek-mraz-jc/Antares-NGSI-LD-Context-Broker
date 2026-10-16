// SPDX-License-Identifier: EUPL-1.2
//! TS 104 175 Table 5.2.12-2: a Subscription's `status` has the allowed
//! values "active", "paused" and "expired". A failed delivery is the
//! Notification's status (5.8.6: "Update notification.status to "failed"";
//! Table 5.2.14.2-1 "ok", "failed"), so the Subscription stays "active"
//! (Coraine subscription_stats_status).

use antares_api::AppState;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use http_body_util::BodyExt;
use serde_json::Value;
use tower::ServiceExt;

async fn send(st: &AppState, method: &str, uri: &str, body: Option<&str>) -> (StatusCode, Value) {
    let mut req = Request::builder().method(method).uri(uri);
    if let Some(b) = body {
        req = req
            .header("Content-Type", "application/json")
            .header("Content-Length", b.len());
    }
    let resp = antares_api::router(st.clone())
        .oneshot(req.body(Body::from(body.unwrap_or_default().to_owned())).expect("req"))
        .await
        .expect("resp");
    let status = resp.status();
    let bytes = resp.into_body().collect().await.expect("body").to_bytes();
    (status, serde_json::from_slice(&bytes).unwrap_or(Value::Null))
}

#[tokio::test(flavor = "multi_thread")]
async fn a_failed_delivery_leaves_the_subscription_active() {
    antares_jsonld::allow_private_egress(true);
    let mut st = AppState::new("test-sub-status".into());
    antares_api::wire(&mut st).await;
    // a closed port: every delivery fails
    let sub = r#"{"id":"urn:S1","type":"Subscription","entities":[{"type":"Vehicle"}],
        "notification":{"endpoint":{"uri":"http://127.0.0.1:9/n"}}}"#;
    let (s, b) = send(&st, "POST", "/ngsi-ld/v1/subscriptions", Some(sub)).await;
    assert_eq!(s, StatusCode::CREATED, "{b}");
    let ent = r#"{"id":"urn:V1","type":"Vehicle","speed":{"type":"Property","value":1}}"#;
    let (s, b) = send(&st, "POST", "/ngsi-ld/v1/entities", Some(ent)).await;
    assert_eq!(s, StatusCode::CREATED, "{b}");

    let mut got = Value::Null;
    for _ in 0..100 {
        (_, got) = send(&st, "GET", "/ngsi-ld/v1/subscriptions/urn:S1", None).await;
        if got["notification"]["status"] == "failed" {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
    assert_eq!(got["notification"]["status"], "failed", "{got}");
    assert_eq!(got["status"], "active", "{got}");
}
