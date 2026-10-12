// SPDX-License-Identifier: EUPL-1.2
//! `/q/retention/{tenant}` and the retention sweep (T-3900): a Tenant's
//! document is checked before it is stored, and the sweep removes, per
//! Tenant and type, the Entities not modified for `current` that carry no
//! `expiresAt` of their own and the instances older than `history`, and
//! nothing of another type or Tenant.

use antares_ops::test_kit::TestBroker;
use axum::body::Body;
use axum::http::{Method, Request, StatusCode};
use axum::Router;
use http_body_util::BodyExt;
use serde_json::{json, Value};
use tower::ServiceExt;

const V: &str = "https://example.org/Vehicle";
const S: &str = "https://example.org/Stop";
const SPEED: &str = "https://example.org/speed";

async fn send(
    app: &Router,
    method: Method,
    uri: &str,
    tenant: Option<&str>,
    body: Option<String>,
) -> (StatusCode, Value) {
    let mut req = Request::builder().method(method).uri(uri);
    if let Some(t) = tenant {
        req = req.header("NGSILD-Tenant", t);
    }
    if let Some(b) = &body {
        req = req
            .header("Content-Type", "application/json")
            .header("Content-Length", b.len());
    }
    let Ok(resp) = app
        .clone()
        .oneshot(
            req.body(Body::from(body.unwrap_or_default()))
                .unwrap_or_default(),
        )
        .await;
    let status = resp.status();
    let bytes = resp
        .into_body()
        .collect()
        .await
        .map(http_body_util::Collected::to_bytes)
        .unwrap_or_default();
    (
        status,
        serde_json::from_slice(&bytes).unwrap_or(Value::Null),
    )
}

/// Now shifted by `minutes` (negative is the past).
fn shift(minutes: i64) -> chrono::DateTime<chrono::Utc> {
    chrono::Utc::now()
        .checked_add_signed(chrono::Duration::minutes(minutes))
        .unwrap_or_default()
}

fn ago(minutes: i64) -> String {
    shift(minutes.saturating_neg()).to_rfc3339_opts(chrono::SecondsFormat::Millis, true)
}

const RULES: &str =
    r#"{"rules":[{"type":"https://example.org/Vehicle","current":"PT5M","history":"5m"}]}"#;

#[tokio::test(flavor = "multi_thread")]
async fn a_document_is_checked_stored_replaced_and_deleted() {
    let b = TestBroker::new().await.expect("broker");
    let app = b.serve_mounted(Vec::new()).expect("app");
    assert_eq!(
        send(&app, Method::GET, "/q/retention/city", None, None)
            .await
            .0,
        StatusCode::NOT_FOUND
    );
    assert_eq!(
        send(
            &app,
            Method::PUT,
            "/q/retention/city",
            None,
            Some(RULES.into())
        )
        .await
        .0,
        StatusCode::CREATED
    );
    let (s, body) = send(&app, Method::GET, "/q/retention/city", None, None).await;
    assert_eq!(
        (s, body["rules"][0]["current"].clone()),
        (StatusCode::OK, json!("PT5M"))
    );
    assert_eq!(
        send(
            &app,
            Method::PUT,
            "/q/retention/city",
            None,
            Some(RULES.into())
        )
        .await
        .0,
        StatusCode::NO_CONTENT
    );
    for bad in [
        r#"{"rules":[{"type":"Vehicle","current":"PT5M"}]}"#,
        r#"{"rules":[{"type":"https://example.org/Vehicle","current":"PT30S"}]}"#,
        r#"{"rules":[{"type":"https://example.org/Vehicle","keep":"PT5M"}]}"#,
        "nope",
    ] {
        let (s, body) = send(
            &app,
            Method::PUT,
            "/q/retention/city",
            None,
            Some(bad.into()),
        )
        .await;
        assert_eq!(
            (s, body["title"].clone()),
            (StatusCode::BAD_REQUEST, json!("BadRequestData")),
            "{bad}"
        );
    }
    assert_eq!(
        send(
            &app,
            Method::PUT,
            "/q/retention/Not%20One",
            None,
            Some(RULES.into())
        )
        .await
        .0,
        StatusCode::BAD_REQUEST
    );
    let (_, body) = send(&app, Method::GET, "/q/retention/city", None, None).await;
    assert_eq!(
        body["rules"][0]["history"], "5m",
        "a refused write left the stored one"
    );
    assert_eq!(
        send(&app, Method::DELETE, "/q/retention/city", None, None)
            .await
            .0,
        StatusCode::NO_CONTENT
    );
    assert_eq!(
        send(&app, Method::DELETE, "/q/retention/city", None, None)
            .await
            .0,
        StatusCode::NOT_FOUND
    );
    assert!(
        b.sweep_retention(shift(1440)).await.is_empty(),
        "no rule, nothing deleted"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_bad_tenant_and_an_oversized_document_are_refused() {
    let b = TestBroker::new().await.expect("broker");
    let app = b.serve_mounted(Vec::new()).expect("app");
    for method in [Method::GET, Method::DELETE] {
        assert_eq!(
            send(&app, method, "/q/retention/Not%20One", None, None)
                .await
                .0,
            StatusCode::BAD_REQUEST
        );
    }
    let big = format!(r#"{{"rules":[],"pad":"{}"}}"#, "x".repeat(64 * 1024));
    assert_eq!(
        send(&app, Method::PUT, "/q/retention/city", None, Some(big))
            .await
            .0,
        StatusCode::PAYLOAD_TOO_LARGE
    );
}

/// Vehicles and a Stop in `city`, a Vehicle in `port`, and two speed
/// instances (6 and 4 minutes old) of `urn:v:hist` in both Tenants.
async fn seed(app: &Router) {
    let app = app.clone();
    let far = shift(1440).to_rfc3339_opts(chrono::SecondsFormat::Millis, true);
    for (tenant, entity) in [
        ("city", json!({"id": "urn:v:1", "type": V})),
        (
            "city",
            json!({"id": "urn:v:own", "type": V, "expiresAt": far}),
        ),
        ("city", json!({"id": "urn:s:1", "type": S})),
        ("port", json!({"id": "urn:v:1", "type": V})),
    ] {
        let (s, body) = send(
            &app,
            Method::POST,
            "/ngsi-ld/v1/entities",
            Some(tenant),
            Some(entity.to_string()),
        )
        .await;
        assert_eq!(s, StatusCode::CREATED, "{tenant} {entity}: {body}");
    }
    for tenant in ["city", "port"] {
        let history = json!({"id": "urn:v:hist", "type": V, SPEED: [
            {"type": "Property", "value": 6, "observedAt": ago(6)},
            {"type": "Property", "value": 4, "observedAt": ago(4)}]});
        let (s, body) = send(
            &app,
            Method::POST,
            "/ngsi-ld/v1/temporal/entities",
            Some(tenant),
            Some(history.to_string()),
        )
        .await;
        assert_eq!(s, StatusCode::CREATED, "{body}");
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn the_sweep_removes_one_type_of_one_tenant_and_a_writers_expiry_wins() {
    let b = TestBroker::new().await.expect("broker");
    let app = b.serve_mounted(Vec::new()).expect("app");
    assert_eq!(
        send(
            &app,
            Method::PUT,
            "/q/retention/city",
            None,
            Some(RULES.into())
        )
        .await
        .0,
        StatusCode::CREATED
    );
    seed(&app).await;
    let instances = |tenant: &'static str| {
        let app = app.clone();
        async move {
            let (_, body) = send(
                &app,
                Method::GET,
                "/ngsi-ld/v1/temporal/entities/urn:v:hist",
                Some(tenant),
                None,
            )
            .await;
            body[SPEED].as_array().map_or(0, Vec::len)
        }
    };

    // Now: the 6-minute instance is past the 5-minute history; every entity was just written.
    let deleted = b.sweep_retention(chrono::Utc::now()).await;
    assert_eq!(
        deleted
            .iter()
            .map(|d| (d.tenant.as_str(), d.kind, d.count))
            .collect::<Vec<_>>(),
        [("city", "history", 1)]
    );
    assert_eq!((instances("city").await, instances("port").await), (1, 2));

    // Ten minutes on: the Vehicle not written since goes, the one with its own expiresAt stays.
    let deleted = b.sweep_retention(shift(10)).await;
    assert_eq!(
        deleted
            .iter()
            .map(|d| (d.tenant.as_str(), d.type_iri.as_str(), d.kind, d.count))
            .collect::<Vec<_>>(),
        [("city", V, "entity", 1), ("city", V, "history", 1)]
    );
    let status = |tenant: &'static str, id: &'static str| {
        let app = app.clone();
        async move {
            send(
                &app,
                Method::GET,
                &format!("/ngsi-ld/v1/entities/{id}"),
                Some(tenant),
                None,
            )
            .await
            .0
        }
    };
    assert_eq!(status("city", "urn:v:1").await, StatusCode::NOT_FOUND);
    assert_eq!(
        status("city", "urn:v:own").await,
        StatusCode::OK,
        "the writer's expiresAt wins"
    );
    assert_eq!(
        status("city", "urn:s:1").await,
        StatusCode::OK,
        "another type keeps everything"
    );
    assert_eq!(
        status("port", "urn:v:1").await,
        StatusCode::OK,
        "another tenant keeps everything"
    );
    assert_eq!(instances("port").await, 2);
}

/// A Vehicle keeps history 5 minutes and current state forever, a Stop the reverse.
const ONE_EACH: &str = r#"{"rules":[{"type":"https://example.org/Vehicle","history":"5m"},
    {"type":"https://example.org/Stop","current":"P1D"}]}"#;

#[tokio::test(flavor = "multi_thread")]
async fn a_backlog_drains_in_batches_within_one_sweep() {
    let b = TestBroker::new().await.expect("broker");
    let app = b.serve_mounted(Vec::new()).expect("app");
    assert_eq!(
        send(
            &app,
            Method::PUT,
            "/q/retention/city",
            None,
            Some(ONE_EACH.into())
        )
        .await
        .0,
        StatusCode::CREATED
    );
    let old: Vec<Value> = (0..2500)
        .map(|i| json!({"type": "Property", "value": i, "observedAt": ago(60 + i64::from(i % 50))}))
        .collect();
    let history = json!({"id": "urn:v:backlog", "type": V, SPEED: old});
    let (s, body) = send(
        &app,
        Method::POST,
        "/ngsi-ld/v1/temporal/entities",
        Some("city"),
        Some(history.to_string()),
    )
    .await;
    assert_eq!(s, StatusCode::CREATED, "{body}");
    let deleted = b.sweep_retention(chrono::Utc::now()).await;
    assert_eq!(
        deleted
            .iter()
            .map(|d| (d.kind, d.count))
            .collect::<Vec<_>>(),
        [("history", 2500)]
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_preview_counts_without_deleting_and_the_totals_follow_the_sweeps() {
    let b = TestBroker::new().await.expect("broker");
    let app = b.serve_mounted(Vec::new()).expect("app");
    seed(&app).await;
    let would = json!([{"type": V, "kind": "history", "count": 1}]);
    for _ in 0..2 {
        let (s, body) = send(
            &app,
            Method::POST,
            "/q/retention/city/preview",
            None,
            Some(RULES.into()),
        )
        .await;
        assert_eq!(
            (s, &body["counts"]),
            (StatusCode::OK, &would),
            "a preview deletes nothing"
        );
    }
    let totals = |tenant: &'static str| {
        let app = app.clone();
        async move {
            send(
                &app,
                Method::GET,
                &format!("/q/retention/{tenant}/deleted"),
                None,
                None,
            )
            .await
            .1["counts"]
                .clone()
        }
    };
    assert_eq!(totals("city").await, json!([]));
    assert_eq!(
        send(
            &app,
            Method::PUT,
            "/q/retention/city",
            None,
            Some(RULES.into())
        )
        .await
        .0,
        StatusCode::CREATED
    );
    assert_eq!(b.sweep_retention(chrono::Utc::now()).await.len(), 1);
    assert_eq!(totals("city").await, would);
    assert_eq!(totals("port").await, json!([]), "per tenant");

    let big = format!(r#"{{"rules":[],"pad":"{}"}}"#, "x".repeat(64 * 1024));
    for (method, uri, body, status) in [
        (
            Method::POST,
            "/q/retention/Not%20One/preview",
            Some(RULES.to_owned()),
            StatusCode::BAD_REQUEST,
        ),
        (
            Method::POST,
            "/q/retention/city/preview",
            Some("nope".to_owned()),
            StatusCode::BAD_REQUEST,
        ),
        (
            Method::POST,
            "/q/retention/city/preview",
            Some(big),
            StatusCode::PAYLOAD_TOO_LARGE,
        ),
        (
            Method::GET,
            "/q/retention/Not%20One/deleted",
            None,
            StatusCode::BAD_REQUEST,
        ),
    ] {
        assert_eq!(send(&app, method, uri, None, body).await.0, status, "{uri}");
    }
}
