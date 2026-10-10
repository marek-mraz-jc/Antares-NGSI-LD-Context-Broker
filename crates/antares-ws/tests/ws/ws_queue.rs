// SPDX-License-Identifier: EUPL-1.2
//! Rule 6: one bounded queue per connection. A consumer that falls behind
//! is closed with `x-gap` and 1013, never buffered without bound, and a
//! single-entity notification without `showChanges` replaces its queued
//! predecessor.

use crate::common::{broker, connect, status, until, with, Client, WAIT};
use antares_ws::Limits;
use serde_json::{json, Value};
use std::time::Duration;

const ROOT: &str = "/x/ws/ngsi-ld/v1";

/// A Subscription to `Car:c`'s `speed`, `showChanges` as given.
fn one_car(show_changes: bool) -> Value {
    json!({"type": "Subscription", "entities": [{"type": "Car", "id": "urn:ngsi-ld:Car:c"}],
           "watchedAttributes": ["speed"], "notification": {"attributes": ["speed"], "showChanges": show_changes}})
}

/// The `speed` values of the notifications that follow until `count` are
/// read.
async fn speeds(c: &mut Client, count: usize) -> Result<Vec<Value>, String> {
    let mut out = Vec::new();
    while out.len() < count {
        let n = c.notification().await?;
        out.push(
            n.pointer("/body/data/0/speed/value")
                .cloned()
                .unwrap_or(Value::Null),
        );
    }
    Ok(out)
}

/// A client that stops reading while 120 Cars of 200 KiB each are created
/// is closed within `pong_within` of the stuck write, the overflow is
/// counted, and it reads far fewer notifications than were matched: the
/// queue held at most `queue` of them.
#[tokio::test(flavor = "multi_thread")]
async fn ws_a_slow_consumer_is_disconnected_not_buffered() -> Result<(), String> {
    const CARS: usize = 120;
    let b = broker(Limits {
        queue: 4,
        pong_within: Duration::from_secs(2),
        ..Limits::default()
    })
    .await?;
    let mut c = connect(b.addr, ROOT, &[]).await?;
    let sub = json!({"type": "Subscription", "entities": [{"type": "Car"}], "watchedAttributes": ["blob"]});
    assert_eq!(status(&c.subscribe(sub).await?), 201);
    let blob = "b".repeat(200 * 1024);
    for i in 0..CARS {
        let body = json!({"id": format!("urn:ngsi-ld:Car:slow{i}"), "type": "Car",
                          "blob": {"type": "Property", "value": blob}});
        let (code, answer) = b
            .http
            .call("POST", "/ngsi-ld/v1/entities", &[], Some(&body))
            .await?;
        assert_eq!(code, 201, "{answer}");
    }
    until(&b.hub, |h| h.overflows() >= 1).await?;
    until(&b.hub, |h| h.attached() == 0).await?;
    let mut read = 0usize;
    loop {
        match tokio::time::timeout(WAIT, c.next()).await {
            Ok(Ok(m)) if m.pointer("/metadata/x-op") == Some(&json!("x-notification")) => {
                read = read.saturating_add(1);
            }
            Ok(Ok(_)) => {}
            Ok(Err(_)) => break,
            Err(_) => return Err("the socket stayed open".into()),
        }
    }
    assert!(read < CARS, "all {read} notifications were buffered");
    Ok(())
}

/// Five changes to one Car while its connection is detached reach the
/// resumed socket as one notification carrying the last value.
#[tokio::test(flavor = "multi_thread")]
async fn ws_coalescing_keeps_the_latest_single_entity_notification() -> Result<(), String> {
    let b = broker(Limits::default()).await?;
    b.http.car(None, "urn:ngsi-ld:Car:c", 0).await?;
    let mut c = connect(b.addr, ROOT, &[]).await?;
    assert_eq!(status(&c.subscribe(one_car(false)).await?), 201);
    let (connection, token) = (c.connection.clone(), c.token.clone());
    c.close().await;
    until(&b.hub, |h| h.attached() == 0).await?;
    for v in 1..=5 {
        b.http.speed(None, "urn:ngsi-ld:Car:c", v).await?;
    }
    let mut again = connect(b.addr, ROOT, &[]).await?;
    assert_eq!(status(&again.resume(&connection, &token, 0).await?), 200);
    let n = again.notification().await?;
    assert_eq!(
        n.pointer("/body/data/0/speed/value"),
        Some(&json!(5)),
        "{n}"
    );
    assert_eq!(n.pointer("/metadata/x-seq"), Some(&json!(1)), "{n}");
    b.http.speed(None, "urn:ngsi-ld:Car:c", 6).await?;
    assert_eq!(
        speeds(&mut again, 1).await?,
        [json!(6)],
        "the next change after the coalesced one"
    );
    Ok(())
}

/// With `showChanges` every change is its own notification: each carries
/// a `previousValue` the next one needs.
#[tokio::test(flavor = "multi_thread")]
async fn ws_show_changes_is_never_coalesced() -> Result<(), String> {
    let b = broker(Limits::default()).await?;
    b.http.car(None, "urn:ngsi-ld:Car:c", 0).await?;
    let mut c = connect(b.addr, ROOT, &[]).await?;
    let sub = with(
        one_car(true),
        &["notification", "format"],
        json!("normalized"),
    );
    assert_eq!(status(&c.subscribe(sub).await?), 201);
    let (connection, token) = (c.connection.clone(), c.token.clone());
    c.close().await;
    until(&b.hub, |h| h.attached() == 0).await?;
    for v in 1..=5 {
        b.http.speed(None, "urn:ngsi-ld:Car:c", v).await?;
    }
    let mut again = connect(b.addr, ROOT, &[]).await?;
    assert_eq!(status(&again.resume(&connection, &token, 0).await?), 200);
    assert_eq!(
        speeds(&mut again, 5).await?,
        [1, 2, 3, 4, 5].map(|v| json!(v))
    );
    Ok(())
}
