// SPDX-License-Identifier: EUPL-1.2
//! A Subscription's `endpoint.receiverInfo` carries the credential its
//! notifications are sent with (6.3.8): no read answers it, and a client
//! writing back what it read keeps the stored credential (T-5565, CC-06).

use antares_api::AppState;
use antares_model::TenantId;
use antares_store::Kind;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use serde_json::{json, Value};
use tower::ServiceExt;

const ID: &str = "urn:ngsi-ld:Subscription:t5565";
const TOKEN: &str = "Bearer the-webhook-token";

async fn send(st: &AppState, method: &str, path: &str, body: Option<Value>) -> (StatusCode, Value) {
    let bytes = body.map(|b| b.to_string()).unwrap_or_default();
    let mut req = Request::builder()
        .method(method)
        .uri(path)
        .header(axum::http::header::CONTENT_LENGTH, bytes.len().to_string());
    if !bytes.is_empty() {
        let ct = if method == "PATCH" {
            "application/merge-patch+json"
        } else {
            "application/json"
        };
        req = req.header(axum::http::header::CONTENT_TYPE, ct);
    }
    let res = antares_api::router(st.clone())
        .oneshot(req.body(Body::from(bytes)).expect("request"))
        .await
        .expect("response");
    let status = res.status();
    let bytes = axum::body::to_bytes(res.into_body(), usize::MAX)
        .await
        .expect("body");
    (
        status,
        serde_json::from_slice(&bytes).unwrap_or(Value::Null),
    )
}

async fn subscribed() -> AppState {
    let st = AppState::new("antares-subscription-secrets".into());
    let (status, body) = send(
        &st,
        "POST",
        "/ngsi-ld/v1/subscriptions",
        Some(json!({
            "id": ID,
            "type": "Subscription",
            "entities": [{"type": "Building"}],
            "notification": {"endpoint": {
                "uri": "http://localhost:18099/notify",
                "receiverInfo": [
                    {"key": "Authorization", "value": TOKEN},
                    {"key": "X-Space", "value": "air-quality"}
                ]
            }}
        })),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    st
}

async fn stored_receiver_info(st: &AppState) -> Value {
    let doc = st
        .store
        .get(&TenantId::default(), Kind::Subscription, ID)
        .await
        .expect("store")
        .expect("the subscription is stored");
    doc["notification"]["endpoint"]["receiverInfo"].clone()
}

#[tokio::test]
async fn receiver_info_is_never_answered() {
    let st = subscribed().await;
    let masked = json!([
        {"key": "Authorization", "value": "***"},
        {"key": "X-Space", "value": "***"}
    ]);
    let (status, one) = send(&st, "GET", &format!("/ngsi-ld/v1/subscriptions/{ID}"), None).await;
    assert_eq!(status, StatusCode::OK, "{one}");
    assert_eq!(one["notification"]["endpoint"]["receiverInfo"], masked);
    let (status, all) = send(&st, "GET", "/ngsi-ld/v1/subscriptions", None).await;
    assert_eq!(status, StatusCode::OK, "{all}");
    assert_eq!(all[0]["notification"]["endpoint"]["receiverInfo"], masked);
    for answer in [one, all] {
        assert!(
            !answer.to_string().contains("the-webhook-token"),
            "{answer}"
        );
    }
    assert_eq!(stored_receiver_info(&st).await[0]["value"], json!(TOKEN));
}

#[tokio::test]
async fn a_masked_value_written_back_keeps_the_stored_credential() {
    let st = subscribed().await;
    let (_, read) = send(&st, "GET", &format!("/ngsi-ld/v1/subscriptions/{ID}"), None).await;
    let mut notification = read["notification"].clone();
    // the client changes one header and writes the rest back as it read it
    notification["endpoint"]["receiverInfo"][1]["value"] = json!("parking");
    let (status, body) = send(
        &st,
        "PATCH",
        &format!("/ngsi-ld/v1/subscriptions/{ID}"),
        Some(json!({ "notification": notification })),
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT, "{body}");
    assert_eq!(
        stored_receiver_info(&st).await,
        json!([
            {"key": "Authorization", "value": TOKEN},
            {"key": "X-Space", "value": "parking"}
        ])
    );
}
