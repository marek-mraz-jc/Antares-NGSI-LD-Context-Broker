// SPDX-License-Identifier: EUPL-1.2
//! A temporal query through a view's rules reads the history its window
//! needs (T-4246). A rule that names types, Attributes and windows decides
//! each instance without the Entity's state, so the query's range reaches
//! the store; a rule with a condition still rebuilds the state at each
//! instance from the whole history. On dev, a type-wide query that loaded and
//! replayed every Entity's whole history ran past the gateway's 30 s.
#![cfg(feature = "test-kit")]

use antares_api::policy::{
    Decision, DecisionFuture, Filter, NotifyDecision, Operation, PolicyEngine, Rule, Subject,
    TimeBound, Window,
};
use antares_api::AppState;
use antares_model::{NgsiError, TenantId};
use antares_store::{filter, MutateFn, TemporalDriver};
use axum::body::Body;
use axum::http::{Request, StatusCode};
use http_body_util::BodyExt;
use serde_json::{json, Value};
use std::sync::{Arc, Mutex};
use tower::ServiceExt;

const DC: &str = "https://uri.etsi.org/ngsi-ld/default-context/";
const CAR: &str = "urn:ngsi-ld:Vehicle:car";

/// Narrows every read to the rules it holds.
struct Ruled(Vec<Rule>);

impl PolicyEngine for Ruled {
    fn name(&self) -> &str {
        "ruled"
    }

    fn decide<'a>(&'a self, _s: &'a Subject, _op: &'a Operation<'a>) -> DecisionFuture<'a> {
        Box::pin(std::future::ready(Decision::Filter(Filter {
            rules: self.0.clone(),
            ..Filter::default()
        })))
    }

    fn pre_notify(&self, _s: &Subject, _sub: &Value, _n: &mut Value) -> NotifyDecision {
        NotifyDecision::Deliver
    }
}

/// Speed and zone, one instance a second from 00:00:01 to 00:00:10.
fn evolution() -> Value {
    let at = |s: u32| format!("2026-01-01T00:00:{s:02}Z");
    json!({"id": CAR, "type": "Vehicle",
           "speed": (1..=10).map(|s| json!({"type": "Property", "value": s, "observedAt": at(s)})).collect::<Vec<_>>(),
           "zone": (1..=10).map(|s| json!({"type": "Property", "value": "A", "observedAt": at(s)})).collect::<Vec<_>>()})
}

/// The rule of a platform Policy: its types, its Attributes and the
/// Policy's temporal window, no condition.
fn plain() -> Rule {
    Rule {
        types: vec!["Vehicle".into()],
        attributes: vec!["speed".into()],
        temporal: vec![Window {
            from: Some(TimeBound::At(
                "2026-01-01T00:00:00Z".parse().expect("instant"),
            )),
            to: None,
        }],
        ..Rule::default()
    }
}

fn conditional() -> Rule {
    Rule {
        q: Some(Box::new(antares_ql::parse_q("zone==\"A\"").expect("q"))),
        ..plain()
    }
}

/// Records, for each temporal read, whether it asked the store for a range
/// and for a lastN cap.
struct Recording {
    inner: Arc<dyn TemporalDriver>,
    ranged: Mutex<Vec<(bool, bool)>>,
}

#[async_trait::async_trait]
impl TemporalDriver for Recording {
    async fn temporal_append(
        &self,
        t: &TenantId,
        id: &str,
        shell: &Value,
        additions: &Value,
    ) -> Result<(), NgsiError> {
        self.inner.temporal_append(t, id, shell, additions).await
    }
    async fn query_temporal(
        &self,
        t: &TenantId,
        f: &filter::TemporalFilter<'_>,
    ) -> Result<filter::TemporalOutcome, NgsiError> {
        self.ranged
            .lock()
            .expect("lock")
            .push((f.range.is_some(), f.last_n.is_some()));
        self.inner.query_temporal(t, f).await
    }
    async fn get_temporal(
        &self,
        t: &TenantId,
        id: &str,
        f: &filter::TemporalFilter<'_>,
    ) -> Result<Option<Value>, NgsiError> {
        self.ranged
            .lock()
            .expect("lock")
            .push((f.range.is_some(), f.last_n.is_some()));
        self.inner.get_temporal(t, id, f).await
    }
    async fn get(&self, t: &TenantId, id: &str) -> Result<Option<Value>, NgsiError> {
        self.inner.get(t, id).await
    }
    async fn create(&self, t: &TenantId, id: &str, doc: Value) -> Result<bool, NgsiError> {
        self.inner.create(t, id, doc).await
    }
    async fn upsert(&self, t: &TenantId, id: &str, doc: Value) -> Result<bool, NgsiError> {
        self.inner.upsert(t, id, doc).await
    }
    async fn delete(&self, t: &TenantId, id: &str) -> Result<bool, NgsiError> {
        self.inner.delete(t, id).await
    }
    async fn list(&self, t: &TenantId) -> Result<Vec<Value>, NgsiError> {
        self.inner.list(t).await
    }
    async fn mutate_boxed<'a>(
        &self,
        t: &TenantId,
        id: &str,
        f: MutateFn<'a>,
    ) -> Result<Option<Result<(), ()>>, NgsiError> {
        self.inner.mutate_boxed(t, id, f).await
    }
}

async fn state(rule: Rule) -> (AppState, Arc<Recording>) {
    state_with(rule, &[]).await
}

/// [`state`] with `more` appended to the evolution before the rule applies.
async fn state_with(rule: Rule, more: &[Value]) -> (AppState, Arc<Recording>) {
    let mut st = AppState::new("me".into());
    antares_api::wire(&mut st).await;
    let (status, body) = send(&st, "/ngsi-ld/v1/temporal/entities", Some(evolution())).await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    for doc in more {
        let (status, body) = send(&st, "/ngsi-ld/v1/temporal/entities", Some(doc.clone())).await;
        assert!(status.is_success(), "{status} {body}");
    }
    let recording = Arc::new(Recording {
        inner: st.temporal.clone(),
        ranged: Mutex::new(Vec::new()),
    });
    st.temporal = recording.clone();
    st.policy = Some(Arc::new(Ruled(vec![rule])));
    (st, recording)
}

async fn send(st: &AppState, uri: &str, body: Option<Value>) -> (StatusCode, Value) {
    let b = Request::builder().uri(uri);
    let req = match body {
        Some(v) => {
            let payload = v.to_string();
            b.method("POST")
                .header("Content-Type", "application/json")
                .header("Content-Length", payload.len())
                .body(Body::from(payload))
                .expect("request")
        }
        None => b.method("GET").body(Body::empty()).expect("request"),
    };
    let res = antares_api::router(st.clone())
        .oneshot(req)
        .await
        .expect("response");
    let status = res.status();
    let bytes = res.into_body().collect().await.expect("body").to_bytes();
    (
        status,
        serde_json::from_slice(&bytes).unwrap_or(Value::Null),
    )
}

const AFTER: &str = "timerel=after&timeAt=2026-01-01T00:00:07Z";

/// The speeds an answer shows, in its order.
fn speeds(doc: &Value) -> Vec<i64> {
    doc.get("speed")
        .or_else(|| doc.get(format!("{DC}speed")))
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|i| i["value"].as_i64())
        .collect()
}

#[tokio::test(flavor = "multi_thread")]
async fn a_rule_without_a_condition_reads_only_the_window_and_shows_the_same() {
    let (st, recording) = state(plain()).await;
    let (status, body) = send(
        &st,
        &format!("/ngsi-ld/v1/temporal/entities?type=Vehicle&{AFTER}"),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(speeds(&body[0]), vec![7, 8, 9, 10], "{body}");
    assert!(
        body[0].get("zone").is_none(),
        "the rule does not show zone: {body}"
    );

    let (status, one) = send(
        &st,
        &format!("/ngsi-ld/v1/temporal/entities/{CAR}?{AFTER}"),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{one}");
    assert_eq!(speeds(&one), vec![7, 8, 9, 10], "{one}");

    assert_eq!(
        *recording.ranged.lock().expect("lock"),
        vec![(true, false), (true, false)]
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_rule_with_a_condition_still_reads_the_whole_history() {
    let (st, recording) = state(conditional()).await;
    let (status, body) = send(
        &st,
        &format!("/ngsi-ld/v1/temporal/entities?type=Vehicle&{AFTER}"),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(speeds(&body[0]), vec![7, 8, 9, 10], "{body}");

    let (status, one) = send(
        &st,
        &format!("/ngsi-ld/v1/temporal/entities/{CAR}?{AFTER}"),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{one}");
    assert_eq!(speeds(&one), vec![7, 8, 9, 10], "{one}");

    assert_eq!(
        *recording.ranged.lock().expect("lock"),
        vec![(false, false), (false, false)]
    );
}

/// The newest `n` speeds through the query and the by-id read, and what the
/// store was asked for.
async fn last_two(rule: Rule) -> (Vec<i64>, Vec<i64>, Vec<(bool, bool)>) {
    let (st, recording) = state(rule).await;
    let (status, body) = send(
        &st,
        &format!("/ngsi-ld/v1/temporal/entities?type=Vehicle&{AFTER}&lastN=2"),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let (status, one) = send(
        &st,
        &format!("/ngsi-ld/v1/temporal/entities/{CAR}?{AFTER}&lastN=2"),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{one}");
    let asked = recording.ranged.lock().expect("lock").clone();
    (speeds(&body[0]), speeds(&one), asked)
}

/// Rules whose windows stay open towards now keep the newest instances of
/// what they show, so the store's lastN cap keeps what they would: the cap
/// goes down with the range.
#[tokio::test(flavor = "multi_thread")]
async fn last_n_reaches_the_store_under_rules_that_keep_the_newest() {
    let (query, one, asked) = last_two(plain()).await;
    assert_eq!(query, vec![10, 9]);
    assert_eq!(one, vec![10, 9]);
    assert_eq!(asked, vec![(true, true), (true, true)]);
}

/// A window that closes before now hides the newest instances: a store cap
/// would keep exactly those and the rule would then drop them, so the cap
/// stays in the API and the answer is the newest the rule shows.
#[tokio::test(flavor = "multi_thread")]
async fn a_window_closed_before_now_keeps_last_n_out_of_the_store() {
    let closed = Rule {
        temporal: vec![Window {
            from: None,
            to: Some(TimeBound::At(
                "2026-01-01T00:00:09Z".parse().expect("instant"),
            )),
        }],
        ..plain()
    };
    let (query, one, asked) = last_two(closed).await;
    assert_eq!(query, vec![8, 7]);
    assert_eq!(one, vec![8, 7]);
    assert_eq!(asked, vec![(true, false), (true, false)]);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_condition_keeps_last_n_out_of_the_store() {
    let (query, one, asked) = last_two(conditional()).await;
    assert_eq!(query, vec![10, 9]);
    assert_eq!(one, vec![10, 9]);
    assert_eq!(asked, vec![(false, false), (false, false)]);
}

/// Rule windows judge observedAt. Ordered by createdAt, the newest
/// instances can be a backfill whose observedAt the window drops, so a
/// store cap would answer with nothing: the cap stays in the API.
#[tokio::test(flavor = "multi_thread")]
async fn last_n_by_another_timeproperty_stays_out_of_the_store() {
    let instances: Vec<Value> = [100, 101]
        .iter()
        .map(|v| json!({"type": "Property", "value": v, "observedAt": "2025-12-31T00:00:00Z"}))
        .collect();
    let backfill = json!({"id": CAR, "type": "Vehicle", "speed": instances});
    let (st, recording) = state_with(plain(), &[backfill]).await;
    let q = "timeproperty=createdAt&timerel=after&timeAt=2000-01-01T00:00:00Z&lastN=2";
    let (status, body) = send(
        &st,
        &format!("/ngsi-ld/v1/temporal/entities?type=Vehicle&{q}"),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let query = speeds(&body[0]);
    let (status, one) = send(
        &st,
        &format!("/ngsi-ld/v1/temporal/entities/{CAR}?{q}"),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{one}");
    assert_eq!(query.len(), 2, "{body}");
    assert_eq!(speeds(&one).len(), 2, "{one}");
    assert!(
        query.iter().chain(&speeds(&one)).all(|v| *v <= 10),
        "the window drops the backfill"
    );
    let asked = recording.ranged.lock().expect("lock").clone();
    assert_eq!(asked, vec![(true, false), (true, false)]);
}
