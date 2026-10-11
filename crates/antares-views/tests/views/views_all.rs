// SPDX-License-Identifier: EUPL-1.2
//! ADR-0023 rule 12: a view that selects every type and shows every
//! Attribute answers what the broker answers without a view, in the view's
//! Tenant (T-3666; the view T-3501 runs the ETSI suite through).

use crate::common::{fixture, put_view, send, Answer, READS, SUBSCRIPTIONS, WRITES};
use axum::http::{Method, StatusCode};
use axum::Router;
use serde_json::{json, Value};

const ALL: &str = "/x/views/all/ngsi-ld/v1";

fn all_view() -> Value {
    json!({
        "id": "all", "tenant": "city", "selector": ["*"],
        "rules": [{"types": ["*"], "attributes": ["*"]}],
        "operations": READS.iter().chain(&WRITES).chain(&SUBSCRIPTIONS).collect::<Vec<_>>()
    })
}

/// `doc` without the id a discovery list mints on every answer
/// (`urn:ngsi-ld:EntityTypeList:<uuid>` and the like).
fn minted(mut doc: Value) -> Value {
    let list = doc
        .get("type")
        .and_then(Value::as_str)
        .is_some_and(|t| t.ends_with("List"));
    if list {
        if let Some(o) = doc.as_object_mut() {
            o.remove("id");
        }
    }
    doc
}

async fn direct(app: &Router, path: &str) -> Answer {
    send(
        app,
        Method::GET,
        &format!("/ngsi-ld/v1{path}"),
        &[("NGSILD-Tenant", "city")],
        None,
    )
    .await
}

async fn through(app: &Router, path: &str) -> Answer {
    send(app, Method::GET, &format!("{ALL}{path}"), &[], None).await
}

#[tokio::test(flavor = "multi_thread")]
async fn views_all_answers_what_the_broker_answers() -> Result<(), String> {
    let f = fixture().await?;
    put_view(&f.app, &all_view()).await?;
    for path in [
        "/entities?type=Car",
        "/entities?type=Car,Person,Organization&count=true",
        "/entities?type=Car&q=secret==1",
        "/entities?type=Car&attrs=secret",
        "/entities/urn:ngsi-ld:Car:3",
        "/entities/urn:ngsi-ld:Org:o",
        "/entities/urn:ngsi-ld:Car:1/attrs/secret",
        "/entities/urn:ngsi-ld:Car:1?join=inline&joinLevel=1",
        "/types",
        "/types?details=true",
        "/types/Car",
        "/attributes",
        "/attributes/secret",
        "/temporal/entities?type=Car&timerel=after&timeAt=1970-01-01T00:00:00Z",
        "/temporal/entities/urn:ngsi-ld:Car:3",
    ] {
        let (d, v) = (direct(&f.app, path).await, through(&f.app, path).await);
        assert_eq!(d.status, StatusCode::OK, "{path}: {}", d.text);
        assert_eq!(v.status, d.status, "{path}: {}", v.text);
        assert_eq!(minted(v.json.clone()), minted(d.json.clone()), "{path}");
        assert_eq!(
            v.header("NGSILD-Results-Count"),
            d.header("NGSILD-Results-Count"),
            "{path}"
        );
    }
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn views_all_writes_and_watches_every_attribute() -> Result<(), String> {
    let f = fixture().await?;
    put_view(&f.app, &all_view()).await?;
    let patch = send(
        &f.app,
        Method::PATCH,
        &format!("{ALL}/entities/urn:ngsi-ld:Car:3/attrs"),
        &[],
        Some(&json!({"secret": {"type": "Property", "value": 7}})),
    )
    .await;
    assert_eq!(patch.status, StatusCode::NO_CONTENT, "{}", patch.text);
    let sub = send(
        &f.app,
        Method::POST,
        &format!("{ALL}/subscriptions"),
        &[],
        Some(&json!({
            "type": "Subscription", "entities": [{"type": "Car"}],
            "watchedAttributes": ["secret"],
            "notification": {"endpoint": {"uri": "http://localhost:1/n"}}
        })),
    )
    .await;
    assert_eq!(sub.status, StatusCode::CREATED, "{}", sub.text);
    Ok(())
}
