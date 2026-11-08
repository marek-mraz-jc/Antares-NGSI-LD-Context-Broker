// SPDX-License-Identifier: EUPL-1.2
//! 5.9.2.4: a Context Source Registration whose content the broker refuses
//! is BadRequestData (400), and nothing is stored. One case per member rule
//! of Table 5.2.9-1 and its RegistrationInfo (Table 5.2.10-1) and EntityInfo
//! (5.2.8) members, each changing one member of an otherwise valid body.

use antares_api::AppState;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use http_body_util::BodyExt;
use serde_json::{json, Value};
use tower::ServiceExt;

async fn send(st: &AppState, method: &str, uri: &str, body: &str) -> (StatusCode, Value) {
    let resp = antares_api::router(st.clone())
        .oneshot(
            Request::builder()
                .method(method)
                .uri(uri)
                .header("Content-Type", "application/json")
                .header("Content-Length", body.len())
                .body(Body::from(body.to_owned()))
                .expect("request"),
        )
        .await
        .expect("response");
    let status = resp.status();
    let bytes = resp.into_body().collect().await.expect("body").to_bytes();
    (status, serde_json::from_slice(&bytes).unwrap_or(Value::Null))
}

fn valid() -> Value {
    json!({
        "id": "urn:ngsi-ld:ContextSourceRegistration:refused",
        "type": "ContextSourceRegistration",
        "endpoint": "http://source.example/ngsi-ld",
        "information": [{"entities": [{"type": "Vehicle"}]}]
    })
}

#[tokio::test]
async fn every_refused_member_is_bad_request_data_and_stores_nothing() {
    let st = AppState::new("me".into());
    let cases: Vec<(&str, &str, Value)> = vec![
        ("information", "not an array", json!("x")),
        ("information", "entry not an object", json!(["x"])),
        ("information", "entity not an object", json!([{"entities": ["x"]}])),
        ("information", "empty type", json!([{"entities": [{"type": ""}]}])),
        ("information", "id not a string", json!([{"entities": [{"type": "T", "id": 5}]}])),
        (
            "information",
            "idPattern not a string",
            json!([{"entities": [{"type": "T", "idPattern": 5}]}]),
        ),
        ("contextSourceInfo", "not an array", json!("x")),
        ("id", "not a string", json!(5)),
        ("endpoint", "not a string", json!(5)),
        ("endpoint", "not a URI", json!("not a uri")),
        ("expiresAt", "not a DateTime", json!(5)),
        ("tenant", "not a string", json!(5)),
        ("datasetId", "not an array", json!("x")),
        ("observationInterval", "not an object", json!("x")),
    ];
    for (member, why, value) in cases {
        let mut body = valid();
        body[member] = value;
        let (code, problem) =
            send(&st, "POST", "/ngsi-ld/v1/csourceRegistrations", &body.to_string()).await;
        assert_eq!(code, StatusCode::BAD_REQUEST, "{member} {why}: {problem}");
        assert!(
            problem["type"]
                .as_str()
                .is_some_and(|t| t.ends_with("BadRequestData")),
            "{member} {why}: {problem}"
        );
    }
    let (code, listed) = send(&st, "GET", "/ngsi-ld/v1/csourceRegistrations?type=Vehicle", "").await;
    assert_eq!(code, StatusCode::OK, "{listed}");
    assert_eq!(listed, json!([]), "a refused registration is never stored");
}
