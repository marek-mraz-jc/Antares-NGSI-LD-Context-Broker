// SPDX-License-Identifier: EUPL-1.2
//! Attributes allowed per type (ADR-0023 rule 11, gap G4, T-3528).
//!
//! A flat `pick` is one list for the whole answer: an Attribute allowed on
//! `Car` is served on `User` too, which is the platform's T-1862 leak ("a
//! projection's attributes are not per type, a query over two types
//! leaks"). `Filter::pick_by_type` and `Filter::omit_by_type` key the lists
//! by expanded type IRI. An Entity keeps what its own types allow, the union
//! over its types that have an entry; an Entity none of whose types has an
//! entry keeps its frame only. Every document an answer carries is judged
//! by its own types: the top level, a joined target, a temporal Entity and
//! the Entities of a notification.
#![cfg(feature = "test-kit")]
#![allow(clippy::unwrap_used)] // an unwrap here is the assertion

use antares_api::policy::{
    Decision, DecisionFuture, Filter, NotifyDecision, Operation, PolicyEngine, Subject,
};
use antares_api::AppState;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use http_body_util::BodyExt;
use serde_json::{json, Value};
use std::collections::BTreeMap;
use std::sync::Arc;
use tower::ServiceExt;

const DEFAULT: &str = "https://uri.etsi.org/ngsi-ld/default-context/";

fn iri(term: &str) -> String {
    format!("{DEFAULT}{term}")
}

/// Narrows every read the seam can narrow and every notification with the
/// same Filter; allows everything else, so a test seeds through the router.
struct Narrowing(Filter);

impl PolicyEngine for Narrowing {
    fn name(&self) -> &str {
        "by-type"
    }

    fn decide<'a>(&'a self, _s: &'a Subject, op: &'a Operation<'a>) -> DecisionFuture<'a> {
        let answer = if antares_api::policy::FILTERABLE.contains(&op.clause) {
            Decision::Filter(self.0.clone())
        } else {
            Decision::Allow
        };
        Box::pin(std::future::ready(answer))
    }

    fn pre_notify(&self, _s: &Subject, _sub: &Value, _n: &mut Value) -> NotifyDecision {
        NotifyDecision::Filter(self.0.clone())
    }
}

/// `type → attributes` as a rule writes it: expanded IRIs.
fn rules(entries: &[(&str, &[&str])]) -> BTreeMap<String, Vec<String>> {
    entries
        .iter()
        .map(|(t, attrs)| (iri(t), attrs.iter().map(|a| iri(a)).collect()))
        .collect()
}

fn picks(entries: &[(&str, &[&str])]) -> Filter {
    Filter {
        pick_by_type: rules(entries),
        ..Filter::default()
    }
}

async fn state(f: Filter) -> AppState {
    antares_jsonld::allow_private_egress(true);
    let mut st = AppState::new("me".into()).with_policy(Arc::new(Narrowing(f)));
    antares_api::wire(&mut st).await;
    st
}

async fn send(st: &AppState, method: &str, uri: &str, body: Option<Value>) -> (StatusCode, Value) {
    let mut b = Request::builder().method(method).uri(uri);
    let req = match body {
        Some(v) => {
            let payload = v.to_string();
            b = b
                .header("Content-Type", "application/json")
                .header("Content-Length", payload.len());
            b.body(Body::from(payload)).unwrap()
        }
        None => b.body(Body::empty()).unwrap(),
    };
    let resp = antares_api::router(st.clone()).oneshot(req).await.unwrap();
    let status = resp.status();
    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    let doc = if bytes.is_empty() {
        Value::Null
    } else {
        serde_json::from_slice(&bytes).unwrap_or(Value::Null)
    };
    (status, doc)
}

async fn create(st: &AppState, doc: Value) {
    let (code, body) = send(st, "POST", "/ngsi-ld/v1/entities", Some(doc)).await;
    assert_eq!(code, StatusCode::CREATED, "{body}");
}

fn prop(v: Value) -> Value {
    json!({"type": "Property", "value": v})
}

/// A Car and a User that both carry `colour`, each with its own members.
async fn seed(st: &AppState) {
    create(
        st,
        json!({"id": "urn:ngsi-ld:Car:1", "type": "Car",
               "speed": prop(json!(80)), "colour": prop(json!("red")),
               "ownedBy": {"type": "Relationship", "object": "urn:ngsi-ld:User:1"}}),
    )
    .await;
    create(
        st,
        json!({"id": "urn:ngsi-ld:User:1", "type": "User",
               "name": prop(json!("Ana")), "email": prop(json!("ana@example.org")),
               "colour": prop(json!("blue"))}),
    )
    .await;
}

/// The member names of each Entity of a list, by id.
fn members(list: &Value) -> BTreeMap<String, Vec<String>> {
    list.as_array()
        .unwrap_or_else(|| panic!("not a list: {list}"))
        .iter()
        .map(|e| {
            let mut keys: Vec<String> = e.as_object().unwrap().keys().cloned().collect();
            keys.sort();
            (e["id"].as_str().unwrap().to_owned(), keys)
        })
        .collect()
}

fn keys(names: &[&str]) -> Vec<String> {
    let mut v: Vec<String> = names.iter().map(|s| (*s).to_owned()).collect();
    v.sort();
    v
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn two_types_one_view_each_sees_only_its_own_attributes() {
    let st = state(picks(&[("Car", &["speed"]), ("User", &["name"])])).await;
    seed(&st).await;
    let (code, list) = send(&st, "GET", "/ngsi-ld/v1/entities?type=Car,User", None).await;
    assert_eq!(code, StatusCode::OK, "{list}");
    let got = members(&list);
    assert_eq!(
        got["urn:ngsi-ld:Car:1"],
        keys(&["id", "type", "speed"]),
        "{list}"
    );
    assert_eq!(
        got["urn:ngsi-ld:User:1"],
        keys(&["id", "type", "name"]),
        "{list}"
    );

    // the retrieve and the batch query answer the same
    let (code, user) = send(&st, "GET", "/ngsi-ld/v1/entities/urn:ngsi-ld:User:1", None).await;
    assert_eq!(code, StatusCode::OK, "{user}");
    assert_eq!(
        members(&json!([user]))["urn:ngsi-ld:User:1"],
        keys(&["id", "type", "name"])
    );
    let (code, list) = send(
        &st,
        "POST",
        "/ngsi-ld/v1/entityOperations/query",
        Some(json!({"type": "Query", "entities": [{"type": "Car"}, {"type": "User"}]})),
    )
    .await;
    assert_eq!(code, StatusCode::OK, "{list}");
    assert_eq!(members(&list), got);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn same_iri_visible_on_car_hidden_on_user() {
    let st = state(picks(&[("Car", &["colour"]), ("User", &["name"])])).await;
    seed(&st).await;
    let (_, list) = send(&st, "GET", "/ngsi-ld/v1/entities?type=Car,User", None).await;
    let got = members(&list);
    assert_eq!(
        got["urn:ngsi-ld:Car:1"],
        keys(&["id", "type", "colour"]),
        "{list}"
    );
    assert_eq!(
        got["urn:ngsi-ld:User:1"],
        keys(&["id", "type", "name"]),
        "{list}"
    );
}

/// Two vocabularies, one short name: the rule keys on the IRI, so the Car's
/// `name` from vocabulary A is shown and the `name` from vocabulary B on the
/// same Car is not.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn same_short_name_two_vocabularies_are_different_attributes() {
    let (a, b) = ("https://a.example.org/name", "https://b.example.org/name");
    let st = state(Filter {
        pick_by_type: BTreeMap::from([(iri("Car"), vec![a.to_owned()])]),
        ..Filter::default()
    })
    .await;
    create(
        &st,
        json!({"id": "urn:ngsi-ld:Car:2", "type": "Car",
               a: prop(json!("from a")), b: prop(json!("from b"))}),
    )
    .await;
    let (code, car) = send(&st, "GET", "/ngsi-ld/v1/entities/urn:ngsi-ld:Car:2", None).await;
    assert_eq!(code, StatusCode::OK, "{car}");
    assert_eq!(car[a]["value"], "from a", "{car}");
    assert!(
        car.get(b).is_none(),
        "the other vocabulary's name leaked: {car}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn type_without_entry_serves_id_and_type_only() {
    let st = state(picks(&[("Car", &["speed"])])).await;
    seed(&st).await;
    let (_, list) = send(&st, "GET", "/ngsi-ld/v1/entities?type=User", None).await;
    assert_eq!(
        members(&list)["urn:ngsi-ld:User:1"],
        keys(&["id", "type"]),
        "{list}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn multi_typed_entity_gets_union_of_selected_types_only() {
    let st = state(picks(&[("Car", &["speed"]), ("User", &["name"])])).await;
    create(
        &st,
        json!({"id": "urn:ngsi-ld:Thing:1", "type": ["Car", "User", "Robot"],
               "speed": prop(json!(5)), "name": prop(json!("R2")),
               "email": prop(json!("r2@example.org")), "serial": prop(json!("x"))}),
    )
    .await;
    let (code, doc) = send(&st, "GET", "/ngsi-ld/v1/entities/urn:ngsi-ld:Thing:1", None).await;
    assert_eq!(code, StatusCode::OK, "{doc}");
    assert_eq!(
        members(&json!([doc]))["urn:ngsi-ld:Thing:1"],
        keys(&["id", "type", "speed", "name"]),
        "{doc}"
    );
}

/// An omit is a promise that the Attribute is never served on that type:
/// a Car that is also a User loses what either type omits.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn omit_by_type_removes_what_any_of_the_types_omits() {
    let st = state(Filter {
        omit_by_type: rules(&[("User", &["email"])]),
        ..Filter::default()
    })
    .await;
    seed(&st).await;
    create(
        &st,
        json!({"id": "urn:ngsi-ld:Car:3", "type": ["Car", "User"],
               "speed": prop(json!(5)), "email": prop(json!("c@example.org"))}),
    )
    .await;
    let (_, list) = send(&st, "GET", "/ngsi-ld/v1/entities?type=Car,User", None).await;
    let got = members(&list);
    assert_eq!(
        got["urn:ngsi-ld:User:1"],
        keys(&["id", "type", "name", "colour"]),
        "{list}"
    );
    assert_eq!(
        got["urn:ngsi-ld:Car:3"],
        keys(&["id", "type", "speed"]),
        "{list}"
    );
    assert!(
        got["urn:ngsi-ld:Car:1"].contains(&"speed".to_owned()),
        "{list}"
    );
}

/// 4.5.23 `join`: the target is judged by its own type, so the Car's
/// allowlist never reaches the User it links to, and the User's never
/// reaches back.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn join_target_projected_by_its_own_type() {
    let st = state(picks(&[
        ("Car", &["speed", "ownedBy", "email"]),
        ("User", &["name"]),
    ]))
    .await;
    seed(&st).await;
    for join in ["inline", "flat"] {
        let (code, body) = send(
            &st,
            "GET",
            &format!("/ngsi-ld/v1/entities?type=Car&join={join}&joinLevel=1"),
            None,
        )
        .await;
        assert_eq!(code, StatusCode::OK, "{body}");
        let text = body.to_string();
        assert!(
            text.contains("Ana"),
            "{join}: the target was not embedded: {body}"
        );
        assert!(
            !text.contains("ana@example.org"),
            "{join}: the Car's allowlist served the User's email: {body}"
        );
        assert!(
            !text.contains("blue"),
            "{join}: the User's colour leaked: {body}"
        );
    }
}

/// 5.7.4: the history of each Entity is narrowed by its own type.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn temporal_answers_are_projected_per_type() {
    let st = state(picks(&[("Car", &["speed"]), ("User", &["name"])])).await;
    for (id, ty, attrs) in [
        ("urn:ngsi-ld:Car:t", "Car", vec!["speed", "colour"]),
        ("urn:ngsi-ld:User:t", "User", vec!["name", "colour"]),
    ] {
        let mut doc = json!({"id": id, "type": ty});
        for a in attrs {
            doc[a] = json!([{"type": "Property", "value": 1,
                             "observedAt": "2026-01-01T00:00:00Z"}]);
        }
        let (code, body) = send(&st, "POST", "/ngsi-ld/v1/temporal/entities", Some(doc)).await;
        assert!(code.is_success(), "{body}");
    }
    let (code, list) = send(
        &st,
        "GET",
        "/ngsi-ld/v1/temporal/entities?type=Car,User&timerel=after&timeAt=1970-01-01T00:00:00Z",
        None,
    )
    .await;
    assert_eq!(code, StatusCode::OK, "{list}");
    let got = members(&list);
    assert_eq!(
        got["urn:ngsi-ld:Car:t"],
        keys(&["id", "type", "speed"]),
        "{list}"
    );
    assert_eq!(
        got["urn:ngsi-ld:User:t"],
        keys(&["id", "type", "name"]),
        "{list}"
    );
    let (code, user) = send(
        &st,
        "GET",
        "/ngsi-ld/v1/temporal/entities/urn:ngsi-ld:User:t",
        None,
    )
    .await;
    assert_eq!(code, StatusCode::OK, "{user}");
    assert_eq!(
        members(&json!([user]))["urn:ngsi-ld:User:t"],
        keys(&["id", "type", "name"])
    );
}

/// A capture server yielding the body of each notification POST.
async fn capture() -> (String, tokio::sync::mpsc::Receiver<Value>) {
    let (tx, rx) = tokio::sync::mpsc::channel::<Value>(8);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let app = axum::Router::new().route(
        "/notify",
        axum::routing::post(move |body: axum::body::Bytes| {
            let tx = tx.clone();
            async move {
                let _ = tx
                    .send(serde_json::from_slice(&body).unwrap_or(Value::Null))
                    .await;
                StatusCode::OK
            }
        }),
    );
    tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    (format!("http://{addr}/notify"), rx)
}

/// 5.8.6 with `showChanges`: the notified User keeps its own allowlist,
/// `previousValue` included, and an Attribute only the Car may show never
/// travels on the User.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn notification_previous_value_projected_per_type() {
    let st = state(picks(&[("Car", &["email"]), ("User", &["name"])])).await;
    seed(&st).await;
    let (uri, mut rx) = capture().await;
    let (code, body) = send(
        &st,
        "POST",
        "/ngsi-ld/v1/subscriptions",
        Some(
            json!({"type": "Subscription", "entities": [{"type": "User"}],
                    "notification": {"endpoint": {"uri": uri}, "showChanges": true}}),
        ),
    )
    .await;
    assert_eq!(code, StatusCode::CREATED, "{body}");
    let (code, body) = send(
        &st,
        "PATCH",
        "/ngsi-ld/v1/entities/urn:ngsi-ld:User:1/attrs",
        Some(json!({"name": prop(json!("Ana B")), "email": prop(json!("b@example.org"))})),
    )
    .await;
    assert_eq!(code, StatusCode::NO_CONTENT, "{body}");
    let mut seen = Vec::new();
    while let Ok(Some(n)) =
        tokio::time::timeout(std::time::Duration::from_secs(10), rx.recv()).await
    {
        let text = n.to_string();
        seen.push(n);
        if text.contains("Ana B") {
            break;
        }
    }
    let last = seen.last().cloned().unwrap_or(Value::Null);
    let text = last.to_string();
    assert!(
        text.contains("Ana B"),
        "no notification of the change: {seen:?}"
    );
    assert!(
        text.contains("previousValue"),
        "showChanges sent no previousValue: {last}"
    );
    assert!(
        !text.contains("example.org") && !text.contains("email"),
        "an Attribute allowed only on Car travelled on the User: {last}"
    );
}

/// The `NGSILD-Warning` of a GET, a header [`send`] does not return.
async fn warning(st: &AppState, uri: &str) -> Option<String> {
    antares_api::router(st.clone())
        .oneshot(Request::builder().uri(uri).body(Body::empty()).unwrap())
        .await
        .unwrap()
        .headers()
        .get("NGSILD-Warning")
        .and_then(|v| v.to_str().ok())
        .map(str::to_owned)
}

fn ids(list: &Value) -> Vec<String> {
    members(list).into_keys().collect()
}

/// ADR-0023 rule 2 per type (T-1862): `colour` is visible on Car and hidden
/// on User, so a `q`, `attrs` or `orderBy` on it may not select, test or
/// order Users. The Users are left out, not the whole answer, and the
/// warning names their type.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn q_on_an_attribute_hidden_for_user_drops_user_not_car() {
    let st = state(picks(&[("Car", &["speed", "colour"]), ("User", &["name"])])).await;
    seed(&st).await;
    for query in [
        "q=colour!=%22green%22",
        "q=colour",
        "q=!colour",
        "attrs=colour",
        "orderBy=colour",
        "pick=id,colour",
    ] {
        let uri = format!("/ngsi-ld/v1/entities?type=Car,User&{query}");
        let (code, list) = send(&st, "GET", &uri, None).await;
        assert_eq!(code, StatusCode::OK, "{query}: {list}");
        let got = ids(&list);
        assert!(
            !got.contains(&"urn:ngsi-ld:User:1".to_owned()),
            "{query}: a User was read through Car's allowlist: {list}"
        );
        if query != "q=!colour" {
            assert_eq!(got, ["urn:ngsi-ld:Car:1"], "{query}: {list}");
        }
        let said = warning(&st, &uri).await.unwrap_or_default();
        assert!(
            said.starts_with("199 ") && said.contains("User"),
            "{query}: warning {said:?}"
        );
        assert!(
            !said.contains("Car,") && !said.contains(" Car "),
            "{query}: Car named: {said:?}"
        );
    }

    // count and the batch query see the same set
    let (_, list) = send(
        &st,
        "GET",
        "/ngsi-ld/v1/entities?type=Car,User&q=colour&count=true&limit=0",
        None,
    )
    .await;
    assert_eq!(list, json!([]));
    let count = antares_api::router(st.clone())
        .oneshot(
            Request::builder()
                .uri("/ngsi-ld/v1/entities?type=Car,User&q=colour&count=true&limit=0")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap()
        .headers()
        .get("NGSILD-Results-Count")
        .and_then(|v| v.to_str().ok())
        .map(str::to_owned);
    assert_eq!(count.as_deref(), Some("1"));
    let (code, list) = send(
        &st,
        "POST",
        "/ngsi-ld/v1/entityOperations/query",
        Some(json!({"type": "Query", "entities": [{"type": "Car"}, {"type": "User"}], "q": "colour"})),
    )
    .await;
    assert_eq!(code, StatusCode::OK, "{list}");
    assert_eq!(ids(&list), ["urn:ngsi-ld:Car:1"], "{list}");

    // a query on what each type shows answers both, with no warning
    let uri = "/ngsi-ld/v1/entities?type=Car,User";
    assert_eq!(ids(&send(&st, "GET", uri, None).await.1).len(), 2);
    assert_eq!(warning(&st, uri).await, None);
}

/// The engine may condition on what it hides: its own `q` is not the
/// client's read.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_engines_own_q_on_a_hidden_attribute_drops_nothing() {
    let st = state(Filter {
        q: Some(Box::new(antares_ql::parse_q("colour").unwrap())),
        ..picks(&[("Car", &["speed"]), ("User", &["name"])])
    })
    .await;
    seed(&st).await;
    let (_, list) = send(&st, "GET", "/ngsi-ld/v1/entities?type=Car,User", None).await;
    let got = members(&list);
    assert_eq!(
        got["urn:ngsi-ld:Car:1"],
        keys(&["id", "type", "speed"]),
        "{list}"
    );
    assert_eq!(
        got["urn:ngsi-ld:User:1"],
        keys(&["id", "type", "name"]),
        "{list}"
    );
}

/// 5.7.4: the temporal query applies the same rule.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_temporal_q_on_an_attribute_hidden_for_user_drops_user() {
    let st = state(picks(&[("Car", &["colour"]), ("User", &["name"])])).await;
    for (id, ty) in [("urn:ngsi-ld:Car:t", "Car"), ("urn:ngsi-ld:User:t", "User")] {
        let (code, body) = send(
            &st,
            "POST",
            "/ngsi-ld/v1/temporal/entities",
            Some(json!({"id": id, "type": ty,
                        "colour": [{"type": "Property", "value": "red", "observedAt": "2026-01-01T00:00:00Z"}],
                        "name": [{"type": "Property", "value": "n", "observedAt": "2026-01-01T00:00:00Z"}]})),
        )
        .await;
        assert!(code.is_success(), "{body}");
    }
    let (code, list) = send(
        &st,
        "GET",
        "/ngsi-ld/v1/temporal/entities?type=Car,User&q=colour==%22red%22&timerel=after&timeAt=1970-01-01T00:00:00Z",
        None,
    )
    .await;
    assert_eq!(code, StatusCode::OK, "{list}");
    assert_eq!(ids(&list), ["urn:ngsi-ld:Car:t"], "{list}");
}

/// The property, exhaustively over a small universe: for every allowlist
/// map over two types and three Attributes, and every Entity typed with a
/// subset of the two, no answer holds an Attribute outside the union of
/// its types' entries.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn no_answer_holds_a_type_attribute_pair_outside_the_map() {
    let attrs = ["a1", "a2", "a3"];
    let subsets = |n: usize| (0..1u8 << n).collect::<Vec<u8>>();
    for car in subsets(3) {
        for user in subsets(3) {
            let pick = |mask: u8| -> Vec<&str> {
                attrs
                    .iter()
                    .enumerate()
                    .filter(|(i, _)| mask & (1 << i) != 0)
                    .map(|(_, a)| *a)
                    .collect()
            };
            let (car_l, user_l) = (pick(car), pick(user));
            let st = state(picks(&[("Car", &car_l), ("User", &user_l)])).await;
            for (i, types) in [
                vec!["Car"],
                vec!["User"],
                vec!["Car", "User"],
                vec!["Robot"],
            ]
            .iter()
            .enumerate()
            {
                let mut doc = json!({"id": format!("urn:ngsi-ld:T:{i}"), "type": types});
                for a in attrs {
                    doc[a] = prop(json!(1));
                }
                create(&st, doc).await;
            }
            let (code, list) =
                send(&st, "GET", "/ngsi-ld/v1/entities?type=Car,User,Robot", None).await;
            assert_eq!(code, StatusCode::OK, "{list}");
            for e in list.as_array().unwrap() {
                let types: Vec<&str> = match &e["type"] {
                    Value::String(t) => vec![t.as_str()],
                    Value::Array(a) => a.iter().filter_map(Value::as_str).collect(),
                    _ => Vec::new(),
                };
                for k in e
                    .as_object()
                    .unwrap()
                    .keys()
                    .filter(|k| !["id", "type"].contains(&k.as_str()))
                {
                    let allowed = (types.contains(&"Car") && car_l.contains(&k.as_str()))
                        || (types.contains(&"User") && user_l.contains(&k.as_str()));
                    assert!(
                        allowed,
                        "car={car_l:?} user={user_l:?}: {k} served on {types:?}: {e}"
                    );
                }
            }
        }
    }
}
