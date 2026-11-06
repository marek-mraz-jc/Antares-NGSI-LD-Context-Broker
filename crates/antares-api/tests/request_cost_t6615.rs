// SPDX-License-Identifier: EUPL-1.2
//! Request cost (T-6615, docs/src/request-cost.md) on the memory store: the
//! two headers with the switch on, none and no accumulator with it off, one
//! pair of headers for a surface answering through in-process calls, and the
//! per-tenant counters. The Postgres half (statements and rows as the store
//! ran them) is tests/request_cost_pg.rs, its own binary for its subscriber.
#![allow(clippy::unwrap_used)] // an unwrap here is the assertion

use antares_api::{ApiSurface, AppState};
use axum::body::Body;
use axum::extract::State;
use axum::http::{HeaderMap, Request, StatusCode};
use axum::response::Response;
use axum::routing::get;
use axum::{Json, Router};
use serde_json::{json, Value};
use tower::ServiceExt;

struct Inner;

impl ApiSurface for Inner {
    fn name(&self) -> &str {
        "rc"
    }
    fn prefix(&self) -> &str {
        "/x/rc"
    }
    fn router(&self, _st: AppState) -> Router<AppState> {
        Router::new()
            .route("/measured", get(measured))
            .route("/nested", get(nested))
    }
    fn version_info(&self) -> Value {
        json!({})
    }
}

/// Whether this request runs under an accumulator.
async fn measured() -> Json<Value> {
    Json(json!({ "measured": antares_store::cost::current().is_some() }))
}

/// Two in-process NGSI-LD reads, one on a task of its own, as a façade does.
async fn nested(State(st): State<AppState>, headers: HeaderMap) -> Json<Value> {
    let read = || {
        Request::get("/ngsi-ld/v1/entities?type=Vehicle")
            .body(Body::empty())
            .unwrap()
    };
    let a = st.call(&headers, read()).await;
    let b = st.call_on_task(None, headers.clone(), read()).await;
    let carried = |r: &Response| r.headers().contains_key("jc-request-cost");
    Json(
        json!({ "innerHeaders": carried(&a) || carried(&b), "status": [a.status().as_u16(), b.status().as_u16()] }),
    )
}

fn state(on: bool) -> AppState {
    let mut st = AppState::new("antares-cost".into())
        .with_surface(Box::new(Inner))
        .unwrap();
    st.request_cost = on;
    st
}

async fn send(st: &AppState, req: Request<Body>) -> (StatusCode, HeaderMap, Value) {
    let res = antares_api::router(st.clone()).oneshot(req).await.unwrap();
    let (parts, body) = res.into_parts();
    let bytes = axum::body::to_bytes(body, usize::MAX).await.unwrap();
    (
        parts.status,
        parts.headers,
        serde_json::from_slice(&bytes).unwrap_or(Value::Null),
    )
}

fn get_req(path: &str, tenant: Option<&str>) -> Request<Body> {
    let mut req = Request::get(path);
    if let Some(t) = tenant {
        req = req.header("NGSILD-Tenant", t);
    }
    req.body(Body::empty()).unwrap()
}

async fn create(st: &AppState, tenant: Option<&str>, id: &str) {
    let body =
        json!({"id": id, "type": "Vehicle", "speed": {"type": "Property", "value": 1}}).to_string();
    let mut req = Request::post("/ngsi-ld/v1/entities")
        .header("Content-Type", "application/json")
        .header("Content-Length", body.len());
    if let Some(t) = tenant {
        req = req.header("NGSILD-Tenant", t);
    }
    let (status, _, _) = send(st, req.body(Body::from(body)).unwrap()).await;
    assert_eq!(status, StatusCode::CREATED);
}

/// The members of a JC-Request-Cost value.
fn members(h: &HeaderMap) -> Vec<(String, f64)> {
    let all: Vec<_> = h.get_all("jc-request-cost").iter().collect();
    assert_eq!(all.len(), 1, "one JC-Request-Cost header");
    all[0]
        .to_str()
        .unwrap()
        .split(", ")
        .map(|m| {
            let (k, v) = m.split_once('=').unwrap();
            (k.to_owned(), v.parse().unwrap())
        })
        .collect()
}

#[tokio::test]
async fn on_every_answer_carries_both_headers() {
    let st = state(true);
    create(&st, None, "urn:ngsi-ld:Vehicle:rc1").await;
    let (status, h, body) = send(&st, get_req("/ngsi-ld/v1/entities?type=Vehicle", None)).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body.as_array().map(Vec::len), Some(1));
    let keys: Vec<_> = members(&h).into_iter().map(|(k, _)| k).collect();
    assert_eq!(keys, ["cpu-ms", "db-ms", "statements", "rows"]);
    let timing = h.get("server-timing").unwrap().to_str().unwrap();
    let names: Vec<_> = timing
        .split(", ")
        .map(|m| m.split_once(";dur=").unwrap().0)
        .collect();
    assert_eq!(names, ["app", "db", "pool", "total"]);
    // an error answer is measured too
    let (status, h, _) = send(&st, get_req("/ngsi-ld/v1/entities/not-a-uri", None)).await;
    assert!(status.is_client_error());
    assert!(h.contains_key("jc-request-cost"));
    let (_, _, seen) = send(&st, get_req("/x/rc/measured", None)).await;
    assert_eq!(seen["measured"], true);
}

#[tokio::test]
async fn off_adds_no_header_and_measures_nothing() {
    let st = state(false);
    create(&st, None, "urn:ngsi-ld:Vehicle:rc2").await;
    let (status, h, _) = send(&st, get_req("/ngsi-ld/v1/entities?type=Vehicle", None)).await;
    assert_eq!(status, StatusCode::OK);
    assert!(!h.contains_key("jc-request-cost"));
    assert!(!h.contains_key("server-timing"));
    let (_, h, seen) = send(&st, get_req("/x/rc/measured", None)).await;
    assert_eq!(seen["measured"], false, "no accumulator exists when off");
    assert!(!h.contains_key("jc-request-cost"));
}

#[tokio::test]
async fn in_process_calls_add_to_the_outer_request() {
    let st = state(true);
    create(&st, None, "urn:ngsi-ld:Vehicle:rc3").await;
    let (status, h, body) = send(&st, get_req("/x/rc/nested", None)).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["status"], json!([200, 200]));
    assert_eq!(
        body["innerHeaders"], false,
        "inner answers carry no cost header"
    );
    assert_eq!(h.get_all("server-timing").iter().count(), 1);
    let cpu = members(&h)[0].1;
    assert!(
        cpu > 0.0,
        "the two inner reads are busy time of this request"
    );
}

#[test]
fn the_counters_move_by_tenant() {
    let recorder = metrics_exporter_prometheus::PrometheusBuilder::new().build_recorder();
    let handle = recorder.handle();
    metrics::with_local_recorder(&recorder, || {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(async {
            let st = state(true);
            create(&st, Some("rc-a"), "urn:ngsi-ld:Vehicle:a1").await;
            create(&st, Some("rc-b"), "urn:ngsi-ld:Vehicle:b1").await;
            for tenant in ["rc-a", "rc-a", "rc-b"] {
                let (status, _, _) = send(
                    &st,
                    get_req("/ngsi-ld/v1/entities?type=Vehicle", Some(tenant)),
                )
                .await;
                assert_eq!(status, StatusCode::OK);
            }
        });
    });
    let text = handle.render();
    let value = |name: &str, tenant: &str| -> f64 {
        text.lines()
            .find(|l| {
                l.starts_with(&format!("{name}{{"))
                    && l.contains(&format!("tenant=\"{tenant}\""))
                    && l.contains("surface=\"ngsi-ld\"")
            })
            .and_then(|l| l.rsplit(' ').next())
            .and_then(|v| v.parse().ok())
            .unwrap_or_else(|| panic!("{name} for {tenant} in\n{text}"))
    };
    // one create and two reads, one create and one read
    assert_eq!(value("antares_request_cost_requests_total", "rc-a"), 3.0);
    assert_eq!(value("antares_request_cost_requests_total", "rc-b"), 2.0);
    assert!(value("antares_request_cost_response_bytes_total", "rc-a") > 0.0);
    assert!(value("antares_request_cost_app_microseconds_total", "rc-b") >= 0.0);
}
