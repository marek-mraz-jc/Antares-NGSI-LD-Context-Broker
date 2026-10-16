// SPDX-License-Identifier: EUPL-1.2
//! Subscriptions made through a view (ADR-0023 rule 9; surfaces threat
//! model V14, V15, V24): stored as sent with the view beside them, matched
//! and notified under the view as it is now.

use crate::common::{fixture, prop, put_view, send, Answer, Fixture, FLEET};
use axum::http::{Method, StatusCode};
use axum::Router;
use serde_json::{json, Value};
use std::time::Duration;
use tokio::sync::mpsc::Receiver;

/// How long a notification may take to arrive.
const WAIT: Duration = Duration::from_secs(10);

/// An endpoint that records every notification it is sent.
async fn sink() -> Result<(String, Receiver<Value>), String> {
    let (tx, rx) = tokio::sync::mpsc::channel::<Value>(16);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .map_err(|e| e.to_string())?;
    let addr = listener.local_addr().map_err(|e| e.to_string())?;
    let app = Router::new().route(
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
    tokio::spawn(async move { axum::serve(listener, app).await });
    Ok((format!("http://{addr}/notify"), rx))
}

/// The next notification, or why none came.
async fn next(rx: &mut Receiver<Value>) -> Result<Value, String> {
    tokio::time::timeout(WAIT, rx.recv())
        .await
        .map_err(|_| "no notification".to_owned())?
        .ok_or_else(|| "the sink closed".to_owned())
}

/// `method root/subscriptions{rest}` as tenant `city` (a view pins it).
async fn subs(f: &Fixture, method: Method, root: &str, rest: &str, body: Option<&Value>) -> Answer {
    send(
        &f.app,
        method,
        &format!("{root}/subscriptions{rest}"),
        &[("NGSILD-Tenant", "city")],
        body,
    )
    .await
}

/// A Subscription `id` to every Car, notifying `uri` with changes shown.
fn to_cars(id: &str, uri: &str) -> Value {
    json!({
        "id": id, "type": "Subscription", "entities": [{"type": "Car"}],
        "notification": {"endpoint": {"uri": uri}, "showChanges": true}
    })
}

/// `sub` with `key` set to `value` in the object at `at`.
fn with(mut sub: Value, at: &str, key: &str, value: Value) -> Value {
    if let Some(o) = sub.pointer_mut(at).and_then(Value::as_object_mut) {
        o.insert(key.into(), value);
    }
    sub
}

/// Make `sub` through `root`.
async fn subscribe(f: &Fixture, root: &str, sub: &Value) -> Result<(), String> {
    let a = subs(f, Method::POST, root, "", Some(sub)).await;
    if a.status == StatusCode::CREATED {
        Ok(())
    } else {
        Err(format!("subscribe: {} {}", a.status, a.text))
    }
}

/// Patch `attrs` of Car `id` through the root.
async fn patch(f: &Fixture, id: &str, attrs: &Value) -> Result<(), String> {
    let a = send(
        &f.app,
        Method::PATCH,
        &format!("/ngsi-ld/v1/entities/urn:ngsi-ld:Car:{id}/attrs"),
        &[("NGSILD-Tenant", "city")],
        Some(attrs),
    )
    .await;
    if a.status == StatusCode::NO_CONTENT {
        Ok(())
    } else {
        Err(format!("patch Car:{id}: {} {}", a.status, a.text))
    }
}

/// The one Entity a notification carries.
fn only(n: &Value) -> Result<&Value, String> {
    match n.get("data").and_then(Value::as_array).map(Vec::as_slice) {
        Some([e]) => Ok(e),
        _ => Err(format!("not one Entity: {n}")),
    }
}

/// Whether `hidden` shows in the Entities `n` carries. The envelope is left out: its random
/// Notification id and timestamps once held the old hidden salary "4321" (T-4250, fast run
/// 38860860930: urn:ngsi-ld:Notification:3012fe29-bdc5-4321-...).
fn leaks(n: &Value, hidden: &str) -> bool {
    n.get("data")
        .is_some_and(|d| d.to_string().contains(hidden))
}

#[test]
fn a_hidden_value_in_the_envelope_alone_is_no_leak() {
    let n = json!({
        "id": "urn:ngsi-ld:Notification:3012fe29-bdc5-4321-9876-543210987654",
        "notifiedAt": "2026-10-16T09:43:21.987654Z",
        "data": [{"id": "urn:ngsi-ld:Car:1", "colour": {"type": "Property", "value": "green"}}],
    });
    assert!(!leaks(&n, "4321") && !leaks(&n, "987654"));
    assert!(leaks(&n, "green"));
}

/// `timesSent` of Subscription `id`, read through `root`.
async fn times_sent(f: &Fixture, root: &str, id: &str) -> Option<u64> {
    let a = subs(f, Method::GET, root, &format!("/{id}"), None).await;
    a.json
        .pointer("/notification/timesSent")
        .and_then(Value::as_u64)
}

/// V14, V15: a change only to hidden Attributes, or to an Entity the view
/// does not select, is not notified; a mixed change is notified with the
/// shown Attributes only, `previousValue` included, the hidden one's not.
#[tokio::test(flavor = "multi_thread")]
async fn views_notifications_drop_hidden_changes_and_strip_previous_value() -> Result<(), String> {
    let f = fixture().await?;
    let (uri, mut rx) = sink().await?;
    subscribe(&f, FLEET, &to_cars("urn:ngsi-ld:Subscription:v14", &uri)).await?;
    patch(&f, "1", &json!({"secret": prop(&json!(2))})).await?;
    patch(&f, "3", &json!({"colour": prop(&json!("grey"))})).await?;
    patch(
        &f,
        "1",
        &json!({"secret": prop(&json!(987_654.5)), "colour": prop(&json!("green"))}),
    )
    .await?;
    let n = next(&mut rx).await?;
    let car = only(&n)?;
    assert_eq!(car.get("id"), Some(&json!("urn:ngsi-ld:Car:1")), "{n}");
    assert_eq!(car.pointer("/colour/value"), Some(&json!("green")), "{n}");
    assert_eq!(
        car.pointer("/colour/previousValue"),
        Some(&json!("red")),
        "{n}"
    );
    for hidden in ["secret", "987654", "grey", "Car:3"] {
        assert!(!leaks(&n, hidden), "{hidden} leaked: {n}");
    }
    assert!(car.get("speed").is_some(), "the shown Attributes stay: {n}");
    Ok(())
}

/// V15, rule 6: a notification joins only targets the view shows, and
/// those with their shown Attributes only.
#[tokio::test(flavor = "multi_thread")]
async fn views_notifications_join_only_what_the_view_shows() -> Result<(), String> {
    let f = fixture().await?;
    let (uri, mut rx) = sink().await?;
    let sub = with(
        to_cars("urn:ngsi-ld:Subscription:join", &uri),
        "/notification",
        "join",
        json!("inline"),
    );
    subscribe(&f, FLEET, &sub).await?;
    patch(&f, "1", &json!({"colour": prop(&json!("green"))})).await?;
    let n = next(&mut rx).await?;
    let car = only(&n)?;
    assert_eq!(
        car.pointer("/owner/entity/name/value"),
        Some(&json!("Pat")),
        "{n}"
    );
    for hidden in ["987654", "salary", "Acme"] {
        assert!(!leaks(&n, hidden), "{hidden} leaked: {n}");
    }
    Ok(())
}

/// V14: the dropped notification is no notification at all, so the
/// Subscription's counters do not tell a hidden write happened.
#[tokio::test(flavor = "multi_thread")]
async fn a_hidden_only_change_leaves_times_sent_unchanged() -> Result<(), String> {
    let f = fixture().await?;
    let (uri, mut rx) = sink().await?;
    let id = "urn:ngsi-ld:Subscription:timing";
    subscribe(&f, FLEET, &to_cars(id, &uri)).await?;
    patch(&f, "1", &json!({"colour": prop(&json!("green"))})).await?;
    next(&mut rx).await?;
    let before = times_sent(&f, FLEET, id).await;
    assert_eq!(before, Some(1));
    patch(&f, "1", &json!({"secret": prop(&json!(7))})).await?;
    patch(&f, "2", &json!({"colour": prop(&json!("black"))})).await?;
    let n = next(&mut rx).await?;
    assert_eq!(
        only(&n)?.get("id"),
        Some(&json!("urn:ngsi-ld:Car:2")),
        "{n}"
    );
    assert_eq!(
        times_sent(&f, FLEET, id).await,
        Some(2),
        "the hidden write counted"
    );
    Ok(())
}

/// Rule 9: the view is applied at match time, so removing it stops
/// delivery of the Subscriptions made through it.
#[tokio::test(flavor = "multi_thread")]
async fn a_removed_view_stops_delivery() -> Result<(), String> {
    let f = fixture().await?;
    let mut temp = crate::common::fleet_view();
    if let Some(o) = temp.as_object_mut() {
        o.insert("id".into(), json!("temp"));
    }
    put_view(&f.app, &temp).await?;
    let root = "/x/views/temp/ngsi-ld/v1";
    let (through, mut heard) = sink().await?;
    let (plain, mut control) = sink().await?;
    subscribe(
        &f,
        root,
        &to_cars("urn:ngsi-ld:Subscription:temp", &through),
    )
    .await?;
    subscribe(
        &f,
        "/ngsi-ld/v1",
        &to_cars("urn:ngsi-ld:Subscription:root", &plain),
    )
    .await?;
    patch(&f, "1", &json!({"colour": prop(&json!("green"))})).await?;
    next(&mut heard).await?;
    next(&mut control).await?;
    let gone = send(&f.app, Method::DELETE, "/q/views/temp", &[], None).await;
    assert_eq!(gone.status, StatusCode::NO_CONTENT, "{}", gone.text);
    patch(&f, "1", &json!({"colour": prop(&json!("white"))})).await?;
    next(&mut control).await?;
    let late = tokio::time::timeout(Duration::from_millis(500), heard.recv()).await;
    assert!(late.is_err(), "delivered after removal: {late:?}");
    assert_eq!(
        times_sent(&f, "/ngsi-ld/v1", "urn:ngsi-ld:Subscription:temp").await,
        Some(1)
    );
    Ok(())
}

/// Rule 9: `watchedAttributes` is intersected with what the view shows at
/// creation and update; nothing left is 400.
#[tokio::test(flavor = "multi_thread")]
async fn watched_attributes_outside_the_view_are_400() -> Result<(), String> {
    let f = fixture().await?;
    let base = to_cars("urn:ngsi-ld:Subscription:w", "http://127.0.0.1:9/n");
    let sub = with(base.clone(), "", "watchedAttributes", json!(["secret"]));
    let a = subs(&f, Method::POST, FLEET, "", Some(&sub)).await;
    assert_eq!(a.status, StatusCode::BAD_REQUEST, "{}", a.text);
    assert_eq!(
        a.json.get("title"),
        Some(&json!("BadRequestData")),
        "{}",
        a.text
    );
    assert!(!a.text.contains("secret"), "{}", a.text);
    let sub = with(base, "", "watchedAttributes", json!(["secret", "speed"]));
    subscribe(&f, FLEET, &sub).await?;
    let a = subs(&f, Method::GET, FLEET, "/urn:ngsi-ld:Subscription:w", None).await;
    assert_eq!(
        a.json.get("watchedAttributes"),
        Some(&json!(["speed"])),
        "{}",
        a.text
    );
    let a = subs(
        &f,
        Method::PATCH,
        FLEET,
        "/urn:ngsi-ld:Subscription:w",
        Some(&json!({"watchedAttributes": ["salary"]})),
    )
    .await;
    assert_eq!(a.status, StatusCode::BAD_REQUEST, "{}", a.text);
    Ok(())
}

/// V24: a Subscription made through a view is not reachable through
/// another view, and a view does not list the root's; the root sees every
/// Subscription of its tenant.
#[tokio::test(flavor = "multi_thread")]
async fn a_subscription_is_reachable_only_through_its_view() -> Result<(), String> {
    let f = fixture().await?;
    let mut other = crate::common::fleet_view();
    if let Some(o) = other.as_object_mut() {
        o.insert("id".into(), json!("other"));
    }
    put_view(&f.app, &other).await?;
    let elsewhere = "/x/views/other/ngsi-ld/v1";
    let mine = "/urn:ngsi-ld:Subscription:mine";
    subscribe(
        &f,
        FLEET,
        &to_cars("urn:ngsi-ld:Subscription:mine", "http://127.0.0.1:9/n"),
    )
    .await?;
    subscribe(
        &f,
        "/ngsi-ld/v1",
        &to_cars("urn:ngsi-ld:Subscription:root", "http://127.0.0.1:9/n"),
    )
    .await?;
    let ids = |a: &Answer| a.ids();
    assert_eq!(
        ids(&subs(&f, Method::GET, FLEET, "", None).await),
        ["urn:ngsi-ld:Subscription:mine"]
    );
    assert_eq!(
        ids(&subs(&f, Method::GET, elsewhere, "", None).await),
        Vec::<String>::new()
    );
    assert_eq!(
        ids(&subs(&f, Method::GET, "/ngsi-ld/v1", "", None).await).len(),
        2
    );
    for (method, body) in [
        (Method::GET, None),
        (Method::PATCH, Some(json!({"description": "x"}))),
        (Method::DELETE, None),
    ] {
        let a = subs(&f, method.clone(), elsewhere, mine, body.as_ref()).await;
        assert_eq!(a.status, StatusCode::NOT_FOUND, "{method}: {}", a.text);
        let a = subs(
            &f,
            method.clone(),
            FLEET,
            "/urn:ngsi-ld:Subscription:root",
            body.as_ref(),
        )
        .await;
        assert_eq!(
            a.status,
            StatusCode::NOT_FOUND,
            "{method} root's: {}",
            a.text
        );
    }
    let a = subs(&f, Method::GET, "/ngsi-ld/v1", mine, None).await;
    assert_eq!(a.status, StatusCode::OK, "{}", a.text);
    assert!(
        !a.text.contains("__view") && !a.text.contains("fleet"),
        "{}",
        a.text
    );
    let a = subs(&f, Method::DELETE, FLEET, mine, None).await;
    assert_eq!(a.status, StatusCode::NO_CONTENT, "{}", a.text);
    Ok(())
}
