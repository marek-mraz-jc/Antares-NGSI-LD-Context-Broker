// SPDX-License-Identifier: EUPL-1.2
//! ADR-0023 rule 7 through the view `fleet` (threat model V6, V10, V11,
//! V13): the Entity is in the view before the write and after it, every
//! Attribute the write names is one the view shows, a Relationship points
//! at an Entity the view shows, and a refused write leaves the store as it
//! was.

use crate::common::{fixture, prop, rel, send, stored, Answer, FLEET};
use axum::http::{Method, StatusCode};
use serde_json::{json, Value};

const MOVES_OUT: &str = "the write would move the Entity out of this view";
const NOT_WRITABLE: &str = "the write names an Attribute this view does not let it write";
const TARGET: &str = "a Relationship written through this view must point at an Entity it shows";

async fn write(app: &axum::Router, method: Method, path: &str, body: Option<&Value>) -> Answer {
    send(app, method, &format!("{FLEET}{path}"), &[], body).await
}

/// `a` is the 403 refusal `why`, in this broker's own namespace.
fn refused(a: &Answer, why: &str) -> Result<(), String> {
    if a.status == StatusCode::FORBIDDEN
        && at(&a.json, "/type") == "urn:antares:error:AccessDenied"
        && at(&a.json, "/detail") == why
    {
        Ok(())
    } else {
        Err(format!("want 403 {why}, got {} {}", a.status, a.text))
    }
}

/// The member at JSON pointer `path` of `v`, `Null` when there is none.
fn at<'a>(v: &'a Value, path: &str) -> &'a Value {
    v.pointer(path).unwrap_or(&Value::Null)
}

/// The value of `attr` of the stored Entity `id`, as the root serves it.
async fn value(app: &axum::Router, id: &str, attr: &str) -> Value {
    at(&stored(app, id).await.json, &format!("/{attr}/value")).clone()
}

#[tokio::test(flavor = "multi_thread")]
async fn views_writes_check_pre_and_post_image_and_the_allowlist() -> Result<(), String> {
    let f = fixture().await?;
    let car1 = "/entities/urn:ngsi-ld:Car:1";

    // in view before and after, a shown Attribute: written
    let a = write(
        &f.app,
        Method::PATCH,
        &format!("{car1}/attrs"),
        Some(&json!({"speed": prop(&json!(20))})),
    )
    .await;
    assert_eq!(a.status, StatusCode::NO_CONTENT, "{}", a.text);
    assert_eq!(value(&f.app, "urn:ngsi-ld:Car:1", "speed").await, 20);

    pre_image(&f.app).await?;
    post_image(&f.app).await?;
    allowlist(&f.app).await?;
    shown_writes(&f.app).await
}

/// An Entity out of the view is not found, byte for byte as an absent one,
/// for every write, and stays as it was.
async fn pre_image(app: &axum::Router) -> Result<(), String> {
    let hidden = write(
        app,
        Method::PATCH,
        "/entities/urn:ngsi-ld:Car:3/attrs",
        Some(&json!({"speed": prop(&json!(10))})),
    )
    .await;
    let absent = write(
        app,
        Method::PATCH,
        "/entities/urn:ngsi-ld:Car:0/attrs",
        Some(&json!({"speed": prop(&json!(10))})),
    )
    .await;
    assert_eq!(hidden.status, StatusCode::NOT_FOUND, "{}", hidden.text);
    assert_eq!(hidden.text.replace("Car:3", "Car:0"), absent.text);
    assert_eq!(value(app, "urn:ngsi-ld:Car:3", "speed").await, 90);
    // the view decides before the type selector does, so a selector that
    // would not match tells nothing of the hidden Entity either
    let typed = |id: &str| format!("/entities/urn:ngsi-ld:{id}/attrs?type=Person");
    let speed = json!({"speed": prop(&json!(10))});
    let hidden = write(app, Method::PATCH, &typed("Car:3"), Some(&speed)).await;
    let absent = write(app, Method::PATCH, &typed("Car:0"), Some(&speed)).await;
    assert_eq!(hidden.text.replace("Car:3", "Car:0"), absent.text);
    for (method, path, body) in [
        (
            Method::POST,
            "/entities/urn:ngsi-ld:Car:3/attrs",
            Some(json!({"colour": prop(&json!("x"))})),
        ),
        (
            Method::PATCH,
            "/entities/urn:ngsi-ld:Car:3/attrs/speed",
            Some(json!({"value": 1})),
        ),
        (
            Method::PUT,
            "/entities/urn:ngsi-ld:Car:3/attrs/speed",
            Some(prop(&json!(1))),
        ),
        (
            Method::PUT,
            "/entities/urn:ngsi-ld:Car:3/attrs/speed/value",
            Some(json!(1)),
        ),
        (
            Method::DELETE,
            "/entities/urn:ngsi-ld:Car:3/attrs/speed",
            None,
        ),
        (
            Method::PATCH,
            "/entities/urn:ngsi-ld:Car:3",
            Some(json!({"speed": prop(&json!(1))})),
        ),
        (
            Method::PUT,
            "/entities/urn:ngsi-ld:Car:3",
            Some(json!({"id": "urn:ngsi-ld:Car:3", "type": "Car", "speed": prop(&json!(1))})),
        ),
        (Method::DELETE, "/entities/urn:ngsi-ld:Car:3", None),
    ] {
        let a = write(app, method.clone(), path, body.as_ref()).await;
        assert_eq!(
            a.status,
            StatusCode::NOT_FOUND,
            "{method} {path}: {}",
            a.text
        );
        assert!(
            !a.text.contains("secret") && !a.text.contains("type selector"),
            "{}",
            a.text
        );
    }
    let car3 = stored(app, "urn:ngsi-ld:Car:3").await;
    assert_eq!(at(&car3.json, "/speed/value"), 90, "{}", car3.text);
    assert_eq!(at(&car3.json, "/secret/value"), 9, "{}", car3.text);

    Ok(())
}

/// A write that would take the Entity out of the view is refused.
async fn post_image(app: &axum::Router) -> Result<(), String> {
    let car1 = "/entities/urn:ngsi-ld:Car:1";
    for (method, path, body) in [
        (
            Method::PATCH,
            format!("{car1}/attrs"),
            Some(json!({"speed": prop(&json!(95))})),
        ),
        (
            Method::PATCH,
            format!("{car1}/attrs/speed"),
            Some(json!({"value": 95})),
        ),
        (
            Method::PUT,
            format!("{car1}/attrs/speed"),
            Some(prop(&json!(95))),
        ),
        (
            Method::PUT,
            format!("{car1}/attrs/speed/value"),
            Some(json!(95)),
        ),
        (
            Method::PATCH,
            car1.to_owned(),
            Some(json!({"speed": prop(&json!(95))})),
        ),
        (Method::DELETE, format!("{car1}/attrs/speed"), None),
    ] {
        let a = write(app, method.clone(), &path, body.as_ref()).await;
        refused(&a, MOVES_OUT).map_err(|e| format!("{method} {path}: {e}"))?;
    }
    assert_eq!(value(app, "urn:ngsi-ld:Car:1", "speed").await, 20);

    Ok(())
}

/// A hidden Attribute refuses the write whole, present or not.
async fn allowlist(app: &axum::Router) -> Result<(), String> {
    let car1 = "/entities/urn:ngsi-ld:Car:1";
    for (method, path, body) in [
        (
            Method::PATCH,
            format!("{car1}/attrs"),
            Some(json!({"colour": prop(&json!("green")), "secret": prop(&json!(2))})),
        ),
        (
            Method::POST,
            format!("{car1}/attrs"),
            Some(json!({"colour": prop(&json!("green")), "secret": prop(&json!(2))})),
        ),
        (
            Method::POST,
            format!("{car1}/attrs?options=noOverwrite"),
            Some(json!({"secret": prop(&json!(2))})),
        ),
        (
            Method::PATCH,
            format!("{car1}/attrs/secret"),
            Some(json!({"value": 2})),
        ),
        (
            Method::PATCH,
            format!("{car1}/attrs/nothere"),
            Some(json!({"value": 2})),
        ),
        (
            Method::PUT,
            format!("{car1}/attrs/secret/value"),
            Some(json!(2)),
        ),
        (
            Method::PUT,
            format!("{car1}/attrs/nothere/value"),
            Some(json!(2)),
        ),
        (Method::DELETE, format!("{car1}/attrs/secret"), None),
        (Method::DELETE, format!("{car1}/attrs/nothere"), None),
        (
            Method::PATCH,
            car1.to_owned(),
            Some(json!({"secret": prop(&json!(2))})),
        ),
    ] {
        let a = write(app, method.clone(), &path, body.as_ref()).await;
        refused(&a, NOT_WRITABLE).map_err(|e| format!("{method} {path}: {e}"))?;
    }
    let car1_now = stored(app, "urn:ngsi-ld:Car:1").await;
    assert_eq!(
        at(&car1_now.json, "/colour/value"),
        "red",
        "{}",
        car1_now.text
    );
    assert_eq!(at(&car1_now.json, "/secret/value"), 1, "{}", car1_now.text);

    Ok(())
}

/// A shown Attribute goes, a merge writes what the view shows, and a delete
/// needs the Entity in view.
async fn shown_writes(app: &axum::Router) -> Result<(), String> {
    let car1 = "/entities/urn:ngsi-ld:Car:1";
    let a = write(app, Method::DELETE, &format!("{car1}/attrs/colour"), None).await;
    assert_eq!(a.status, StatusCode::NO_CONTENT, "{}", a.text);
    let a = write(
        app,
        Method::PATCH,
        car1,
        Some(&json!({"colour": prop(&json!("green"))})),
    )
    .await;
    assert_eq!(a.status, StatusCode::NO_CONTENT, "{}", a.text);
    assert_eq!(value(app, "urn:ngsi-ld:Car:1", "colour").await, "green");

    // a delete needs the Entity in view; out of view it stays
    let a = write(app, Method::DELETE, "/entities/urn:ngsi-ld:Car:2", None).await;
    assert_eq!(a.status, StatusCode::NO_CONTENT, "{}", a.text);
    assert_eq!(
        stored(app, "urn:ngsi-ld:Car:2").await.status,
        StatusCode::NOT_FOUND
    );
    assert_eq!(
        stored(app, "urn:ngsi-ld:Car:3").await.status,
        StatusCode::OK
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn a_replace_is_refused_when_the_stored_entity_holds_hidden_attributes() -> Result<(), String>
{
    let f = fixture().await?;
    let body = json!({"id": "urn:ngsi-ld:Car:1", "type": "Car", "speed": prop(&json!(5)), "colour": prop(&json!("grey"))});
    let a = write(
        &f.app,
        Method::PUT,
        "/entities/urn:ngsi-ld:Car:1",
        Some(&body),
    )
    .await;
    refused(&a, NOT_WRITABLE)?;
    let car1 = stored(&f.app, "urn:ngsi-ld:Car:1").await;
    assert_eq!(at(&car1.json, "/secret/value"), 1, "{}", car1.text);
    assert_eq!(at(&car1.json, "/speed/value"), 10, "{}", car1.text);

    // an Entity holding only what the view shows is replaced
    let car5 = json!({"id": "urn:ngsi-ld:Car:5", "type": "Car", "speed": prop(&json!(5))});
    let a = write(&f.app, Method::POST, "/entities", Some(&car5)).await;
    assert_eq!(a.status, StatusCode::CREATED, "{}", a.text);
    let a = write(&f.app, Method::PUT, "/entities/urn:ngsi-ld:Car:5", Some(&json!({"id": "urn:ngsi-ld:Car:5", "type": "Car", "speed": prop(&json!(6)), "colour": prop(&json!("grey"))}))).await;
    assert_eq!(a.status, StatusCode::NO_CONTENT, "{}", a.text);
    // and the replacement is judged as any write
    let a = write(
        &f.app,
        Method::PUT,
        "/entities/urn:ngsi-ld:Car:5",
        Some(&json!({"id": "urn:ngsi-ld:Car:5", "type": "Car", "speed": prop(&json!(99))})),
    )
    .await;
    refused(&a, MOVES_OUT)?;
    assert_eq!(value(&f.app, "urn:ngsi-ld:Car:5", "speed").await, 6);
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn a_create_of_an_out_of_view_id_reveals_only_that_the_id_is_taken() -> Result<(), String> {
    let f = fixture().await?;
    let car = |id: &str| json!({"id": format!("urn:ngsi-ld:Car:{id}"), "type": "Car", "speed": prop(&json!(1))});
    let hidden = write(&f.app, Method::POST, "/entities", Some(&car("3"))).await;
    let shown = write(&f.app, Method::POST, "/entities", Some(&car("1"))).await;
    assert_eq!(hidden.status, StatusCode::CONFLICT, "{}", hidden.text);
    assert_eq!(hidden.text.replace("Car:3", "Car:1"), shown.text);
    assert_eq!(value(&f.app, "urn:ngsi-ld:Car:3", "speed").await, 90);

    // a create lands in the view, with what it shows, or not at all
    let a = write(
        &f.app,
        Method::POST,
        "/entities",
        Some(&json!({"id": "urn:ngsi-ld:Car:7", "type": "Car", "speed": prop(&json!(99))})),
    )
    .await;
    refused(&a, MOVES_OUT)?;
    let a = write(
        &f.app,
        Method::POST,
        "/entities",
        Some(&json!({"id": "urn:ngsi-ld:Org:7", "type": "Org"})),
    )
    .await;
    refused(&a, MOVES_OUT)?;
    let a = write(&f.app, Method::POST, "/entities", Some(&json!({"id": "urn:ngsi-ld:Car:7", "type": "Car", "speed": prop(&json!(1)), "secret": prop(&json!(1))}))).await;
    refused(&a, NOT_WRITABLE)?;
    for id in ["urn:ngsi-ld:Car:7", "urn:ngsi-ld:Org:7"] {
        assert_eq!(
            stored(&f.app, id).await.status,
            StatusCode::NOT_FOUND,
            "{id}"
        );
    }
    let a = write(&f.app, Method::POST, "/entities", Some(&car("7"))).await;
    assert_eq!(a.status, StatusCode::CREATED, "{}", a.text);
    assert_eq!(
        a.header("location").as_deref(),
        Some("/x/views/fleet/ngsi-ld/v1/entities/urn:ngsi-ld:Car:7")
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn a_relationship_target_must_be_visible_and_absent_answers_alike() -> Result<(), String> {
    let f = fixture().await?;
    let car1 = "/entities/urn:ngsi-ld:Car:1";
    let a = write(
        &f.app,
        Method::PATCH,
        &format!("{car1}/attrs"),
        Some(&json!({"driver": rel("urn:ngsi-ld:Person:p")})),
    )
    .await;
    assert_eq!(a.status, StatusCode::NO_CONTENT, "{}", a.text);
    for target in [
        "urn:ngsi-ld:Org:o",
        "urn:ngsi-ld:Car:3",
        "urn:ngsi-ld:Person:nobody",
    ] {
        for (method, path, body) in [
            (
                Method::PATCH,
                format!("{car1}/attrs"),
                json!({"owner": rel(target)}),
            ),
            (
                Method::POST,
                format!("{car1}/attrs"),
                json!({"owner": rel(target)}),
            ),
            (
                Method::PATCH,
                format!("{car1}/attrs/owner"),
                json!({"object": target}),
            ),
            (Method::PUT, format!("{car1}/attrs/owner"), rel(target)),
            (
                Method::PATCH,
                car1.to_owned(),
                json!({"owner": rel(target)}),
            ),
            (
                Method::POST,
                "/entities".to_owned(),
                json!({"id": "urn:ngsi-ld:Car:8", "type": "Car", "speed": prop(&json!(1)), "owner": rel(target)}),
            ),
        ] {
            let a = write(&f.app, method.clone(), &path, Some(&body)).await;
            refused(&a, TARGET).map_err(|e| format!("{method} {path} -> {target}: {e}"))?;
        }
    }
    let car1 = stored(&f.app, "urn:ngsi-ld:Car:1").await;
    assert_eq!(
        at(&car1.json, "/owner/object"),
        "urn:ngsi-ld:Person:p",
        "{}",
        car1.text
    );
    assert_eq!(
        stored(&f.app, "urn:ngsi-ld:Car:8").await.status,
        StatusCode::NOT_FOUND
    );
    Ok(())
}

/// One `BatchEntityError` of a 207 answer, by Entity id.
fn entry<'a>(a: &'a Answer, id: &str) -> &'a Value {
    a.json
        .get("errors")
        .and_then(Value::as_array)
        .and_then(|e| e.iter().find(|e| e.get("entityId") == Some(&json!(id))))
        .unwrap_or(&Value::Null)
}

#[tokio::test(flavor = "multi_thread")]
async fn views_batch_is_decided_per_entity() -> Result<(), String> {
    let f = fixture().await?;
    let car = |n: &str, speed: u32| json!({"id": format!("urn:ngsi-ld:Car:{n}"), "type": "Car", "speed": prop(&json!(speed))});
    let ops = "/entityOperations";

    // update: in view written, out of view not found as absent, out of the
    // view after it or a hidden Attribute refused, each on its own
    let mut secret = car("2", 40);
    if let Some(o) = secret.as_object_mut() {
        o.insert("secret".into(), prop(&json!(0)));
    }
    let body = json!([car("1", 30), car("3", 10), car("0", 10), secret]);
    let a = write(&f.app, Method::POST, &format!("{ops}/update"), Some(&body)).await;
    assert_eq!(a.status, StatusCode::MULTI_STATUS, "{}", a.text);
    assert_eq!(
        a.json.get("success"),
        Some(&json!(["urn:ngsi-ld:Car:1"])),
        "{}",
        a.text
    );
    assert_eq!(
        entry(&a, "urn:ngsi-ld:Car:3")
            .to_string()
            .replace("Car:3", "Car:0"),
        entry(&a, "urn:ngsi-ld:Car:0").to_string()
    );
    assert_eq!(
        at(entry(&a, "urn:ngsi-ld:Car:2"), "/error/detail"),
        NOT_WRITABLE,
        "{}",
        a.text
    );
    assert_eq!(value(&f.app, "urn:ngsi-ld:Car:1", "speed").await, 30);
    assert_eq!(value(&f.app, "urn:ngsi-ld:Car:3", "speed").await, 90);
    assert_eq!(value(&f.app, "urn:ngsi-ld:Car:2", "speed").await, 50);

    // merge: the post-image decides per member
    let body = json!([car("1", 31), car("2", 95)]);
    let a = write(&f.app, Method::POST, &format!("{ops}/merge"), Some(&body)).await;
    assert_eq!(a.status, StatusCode::MULTI_STATUS, "{}", a.text);
    assert_eq!(
        at(entry(&a, "urn:ngsi-ld:Car:2"), "/error/detail"),
        MOVES_OUT,
        "{}",
        a.text
    );
    assert_eq!(value(&f.app, "urn:ngsi-ld:Car:1", "speed").await, 31);
    assert_eq!(value(&f.app, "urn:ngsi-ld:Car:2", "speed").await, 50);

    batch_create_and_delete(&f.app).await
}

/// Batch create and delete through `fleet`, member by member.
async fn batch_create_and_delete(app: &axum::Router) -> Result<(), String> {
    let car = |n: &str, speed: u32| json!({"id": format!("urn:ngsi-ld:Car:{n}"), "type": "Car", "speed": prop(&json!(speed))});
    let ops = "/entityOperations";
    // create: in the view, or 403; a taken id is 409 whether shown or not
    let mut owned = car("21", 1);
    if let Some(o) = owned.as_object_mut() {
        o.insert("owner".into(), rel("urn:ngsi-ld:Org:o"));
    }
    let body = json!([car("20", 1), car("22", 99), owned, car("3", 1), car("1", 1)]);
    let a = write(app, Method::POST, &format!("{ops}/create"), Some(&body)).await;
    assert_eq!(a.status, StatusCode::MULTI_STATUS, "{}", a.text);
    assert_eq!(
        a.json.get("success"),
        Some(&json!(["urn:ngsi-ld:Car:20"])),
        "{}",
        a.text
    );
    assert_eq!(
        at(entry(&a, "urn:ngsi-ld:Car:22"), "/error/detail"),
        MOVES_OUT,
        "{}",
        a.text
    );
    assert_eq!(
        at(entry(&a, "urn:ngsi-ld:Car:21"), "/error/detail"),
        TARGET,
        "{}",
        a.text
    );
    assert_eq!(
        entry(&a, "urn:ngsi-ld:Car:3")
            .to_string()
            .replace("Car:3", "Car:1"),
        entry(&a, "urn:ngsi-ld:Car:1").to_string()
    );
    for id in ["urn:ngsi-ld:Car:21", "urn:ngsi-ld:Car:22"] {
        assert_eq!(stored(app, id).await.status, StatusCode::NOT_FOUND, "{id}");
    }

    // delete: in view deleted, out of view not found as absent and kept
    let body = json!([
        "urn:ngsi-ld:Car:20",
        "urn:ngsi-ld:Car:3",
        "urn:ngsi-ld:Car:0"
    ]);
    let a = write(app, Method::POST, &format!("{ops}/delete"), Some(&body)).await;
    assert_eq!(a.status, StatusCode::MULTI_STATUS, "{}", a.text);
    assert_eq!(
        a.json.get("success"),
        Some(&json!(["urn:ngsi-ld:Car:20"])),
        "{}",
        a.text
    );
    assert_eq!(
        entry(&a, "urn:ngsi-ld:Car:3")
            .to_string()
            .replace("Car:3", "Car:0"),
        entry(&a, "urn:ngsi-ld:Car:0").to_string()
    );
    assert_eq!(
        stored(app, "urn:ngsi-ld:Car:3").await.status,
        StatusCode::OK
    );
    Ok(())
}
