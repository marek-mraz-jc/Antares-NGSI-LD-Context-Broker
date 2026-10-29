// SPDX-License-Identifier: EUPL-1.2
//! T-5912: a check a module registers with `Ops::check_writes` holds every
//! NGSI-LD write of a Tenant with a schema, under `ANTARES_SCHEMA_WRITES=enforce`
//! (read when the views loader starts); a Tenant without a schema is written
//! as before.

use antares_ops::test_kit::TestBroker;
use antares_ops::views::{SCHEMA_MAX_ENV, SCHEMA_WRITES_ENV};
use antares_ops::{BuiltForward, ForwardFuture, ForwardTransport};
use axum::body::Body;
use axum::http::{Request, StatusCode};
use axum::Router;
use serde_json::{json, Value};
use std::sync::{Arc, PoisonError};
use tower::ServiceExt;

/// Forwards nothing in process: every forward goes over HTTP.
struct Http;

impl ForwardTransport for Http {
    fn forward<'a>(&'a self, _: BuiltForward<'a>) -> Option<ForwardFuture<'a>> {
        None
    }
}

/// Refuses an Entity holding `bad`, by its text alone.
fn no_bad(_: u64, _: &Arc<[u8]>, doc: &Value, _: bool) -> Result<(), String> {
    if doc.to_string().contains("bad") {
        Err("the attribute n breaks the data model of T: no".into())
    } else {
        Ok(())
    }
}

async fn send(
    app: &Router,
    method: &str,
    path: &str,
    tenant: &str,
    doc: &Value,
) -> (StatusCode, Value) {
    let payload = doc.to_string();
    let req = Request::builder()
        .method(method)
        .uri(path)
        .header("NGSILD-Tenant", tenant)
        .header("Content-Type", "application/json")
        .header("Content-Length", payload.len())
        .body(Body::from(payload))
        .unwrap_or_else(|e| unreachable!("{e}"));
    let answer = app
        .clone()
        .oneshot(req)
        .await
        .unwrap_or_else(|e| unreachable!("{e}"));
    let status = answer.status();
    let bytes = axum::body::to_bytes(answer.into_body(), usize::MAX)
        .await
        .unwrap_or_default();
    (
        status,
        serde_json::from_slice(&bytes).unwrap_or(Value::Null),
    )
}

#[tokio::test(flavor = "multi_thread")]
async fn a_registered_check_refuses_a_write_to_a_tenant_with_a_schema() {
    std::env::set_var(SCHEMA_MAX_ENV, "65536");
    std::env::set_var(SCHEMA_WRITES_ENV, "enforce");
    let kept = Arc::new(std::sync::Mutex::new(None));
    let keep = kept.clone();
    let b = TestBroker::new()
        .await
        .unwrap_or_else(|e| unreachable!("{e}"))
        .with_forward_transport(move |ops| {
            ops.check_writes(no_bad);
            *keep.lock().unwrap_or_else(PoisonError::into_inner) = Some(());
            Arc::new(Http)
        })
        .unwrap_or_else(|e| unreachable!("{e}"));
    assert!(kept
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .is_some());
    let app = b.router();
    let schema = json!({"id": "https://e.org/", "name": "s"});
    let (s, body) = send(&app, "PUT", "/q/schemas/held", "held", &schema).await;
    assert!(s.is_success(), "{s} {body}");
    let bad =
        json!({"id": "urn:ngsi-ld:T:1", "type": "T", "n": {"type": "Property", "value": "bad"}});
    let (s, body) = send(&app, "POST", "/ngsi-ld/v1/entities", "held", &bad).await;
    assert_eq!(s, StatusCode::BAD_REQUEST, "{body}");
    assert_eq!(
        body["detail"], "the attribute n breaks the data model of T: no",
        "{body}"
    );
    let (s, body) = send(&app, "POST", "/ngsi-ld/v1/entities", "free", &bad).await;
    assert_eq!(s, StatusCode::CREATED, "a Tenant without a schema: {body}");
}
