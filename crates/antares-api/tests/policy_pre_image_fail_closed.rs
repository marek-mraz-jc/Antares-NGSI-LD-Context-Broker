// SPDX-License-Identifier: EUPL-1.2
//! A pre-image the store cannot read fails the write (ADR-0020 fail-closed,
//! F-ANT2-009). Before, the failed read became an absent Entity and an
//! engine keyed on the stored types decided over nothing: a rule that
//! refuses writes to a `Secret` Entity let them through.
#![allow(clippy::unwrap_used)] // an unwrap here is the assertion

use crate::common;

use antares_api::policy::{
    Decision, DecisionFuture, NotifyDecision, Operation, PolicyEngine, Subject,
};
use antares_api::AppState;
use antares_store::Kind;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use common::Double;
use serde_json::{json, Value};
use std::sync::Arc;
use tower::ServiceExt;

const ID: &str = "urn:ngsi-ld:Secret:1";

/// Refuses any write whose stored pre-image is a `Secret`.
struct NoSecrets;

impl PolicyEngine for NoSecrets {
    fn name(&self) -> &str {
        "no-secrets"
    }
    fn decide<'a>(&'a self, _s: &'a Subject, op: &'a Operation<'a>) -> DecisionFuture<'a> {
        let secret = op
            .stored
            .is_some_and(|s| s.types.iter().any(|t| t.ends_with("Secret")));
        Box::pin(std::future::ready(if secret {
            Decision::Deny("secret".into())
        } else {
            Decision::Allow
        }))
    }
    fn pre_notify(&self, _s: &Subject, _sub: &Value, _n: &mut Value) -> NotifyDecision {
        NotifyDecision::Deliver
    }
}

async fn send(st: &AppState, method: &str, uri: &str, body: &str) -> StatusCode {
    antares_api::router(st.clone())
        .oneshot(
            Request::builder()
                .method(method)
                .uri(uri)
                .header("Content-Type", "application/json")
                .header("Content-Length", body.len())
                .body(Body::from(body.to_owned()))
                .unwrap(),
        )
        .await
        .unwrap()
        .status()
}

async fn seeded() -> AppState {
    let st = AppState::new("me".into());
    let e = json!({"id": ID, "type": "Secret", "pin": {"type": "Property", "value": 1}});
    assert_eq!(
        send(&st, "POST", "/ngsi-ld/v1/entities", &e.to_string()).await,
        StatusCode::CREATED
    );
    st
}

#[tokio::test]
async fn an_unreadable_pre_image_fails_the_write_instead_of_allowing_it() {
    let st = seeded().await;
    let patch = json!({"pin": {"type": "Property", "value": 2}}).to_string();
    let uri = format!("/ngsi-ld/v1/entities/{ID}/attrs");
    // readable: the engine sees the Secret and refuses
    let guarded = st.clone().with_policy(Arc::new(NoSecrets));
    assert_eq!(
        send(&guarded, "PATCH", &uri, &patch).await,
        StatusCode::FORBIDDEN
    );
    // unreadable: the request fails, it is not decided over an absent Entity
    let mut broken = guarded.clone();
    broken.store = Arc::new(Double::refusing_doc(st.store.clone(), Kind::Entity, ID));
    assert_eq!(
        send(&broken, "PATCH", &uri, &patch).await,
        StatusCode::INTERNAL_SERVER_ERROR
    );
    let doc = st
        .store
        .get(&Default::default(), Kind::Entity, ID)
        .await
        .unwrap()
        .unwrap();
    assert!(
        doc.to_string().contains("\"value\":1"),
        "the write never ran: {doc}"
    );
}

/// The batch delete reads every pre-image in one call and fails closed the
/// same way (F-ANT2-025).
#[tokio::test]
async fn a_batch_delete_with_an_unreadable_pre_image_fails_whole() {
    let st = seeded().await;
    let mut broken = st.clone().with_policy(Arc::new(NoSecrets));
    broken.store = Arc::new(Double::refusing_doc(st.store.clone(), Kind::Entity, ID));
    let body = json!([ID]).to_string();
    assert_eq!(
        send(
            &broken,
            "POST",
            "/ngsi-ld/v1/entityOperations/delete",
            &body
        )
        .await,
        StatusCode::INTERNAL_SERVER_ERROR
    );
    assert!(st
        .store
        .get(&Default::default(), Kind::Entity, ID)
        .await
        .unwrap()
        .is_some());
}

/// A batch delete waits for a batch slot like the write batches (F-ANT2-026).
#[tokio::test]
async fn a_batch_delete_waits_for_a_batch_slot() {
    let st = seeded().await;
    let mut held = Vec::new();
    for _ in 0..*antares_api::bounds::BATCH_INFLIGHT {
        held.push(antares_api::bounds::admit_batch(0).await.unwrap());
    }
    let body = json!([ID]).to_string();
    let call = {
        let st = st.clone();
        tokio::spawn(async move {
            send(&st, "POST", "/ngsi-ld/v1/entityOperations/delete", &body).await
        })
    };
    tokio::time::sleep(std::time::Duration::from_millis(300)).await;
    assert!(!call.is_finished(), "ran without a slot");
    drop(held);
    assert_eq!(call.await.unwrap(), StatusCode::NO_CONTENT);
}
