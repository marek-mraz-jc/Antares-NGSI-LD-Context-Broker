// SPDX-License-Identifier: EUPL-1.2
//! Writes of one Entity (odata-linkml-mapping section 6 rows W1 to W9,
//! W15, W32; surfaces-threat-model O8, O9): each one gated NGSI-LD
//! operation of the view; a bind reads its target under the view first, so
//! a hidden target and an absent one answer the same 404 and nothing is
//! written; the forms no operation stands behind are refused before any.

mod writes;

use axum::body::Body;
use axum::http::{Method, Request, StatusCode};
use serde_json::{json, Value};
use tower::ServiceExt;
use writes::{city, odata, send, stored, CITY, ROOT};

#[tokio::test(flavor = "multi_thread")]
async fn odata_writes_an_entity_through_its_view() {
    let (_b, app) = city().await.expect("broker");
    let created = r#"{"id": "urn:ngsi-ld:Car:7", "speed": 3, "label": "seven", "scope": ["/a"],
                      "owner@odata.bind": "Person('urn:ngsi-ld:Person:1')"}"#;
    let (status, body) = odata(&app, Method::POST, "Car", Some(created)).await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    assert_eq!(
        (body["id"].as_str(), body["speed"].as_f64()),
        (Some("urn:ngsi-ld:Car:7"), Some(3.0)),
        "{body}"
    );
    let (_, stored_car) = stored(&app, "urn:ngsi-ld:Car:7").await;
    assert_eq!(
        stored_car[format!("{CITY}owner")]["object"],
        "urn:ngsi-ld:Person:1",
        "the bind is a Relationship: {stored_car}"
    );
    assert_eq!(stored_car["type"], format!("{CITY}Car"));

    let one = "Car('urn:ngsi-ld:Car:7')";
    let (status, body) = odata(
        &app,
        Method::PATCH,
        one,
        Some(r#"{"speed": 5, "label": null}"#),
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT, "{body}");
    let (_, car) = odata(&app, Method::GET, one, None).await;
    assert_eq!(
        (car["speed"].as_f64(), &car["label"]),
        (Some(5.0), &Value::Null),
        "a merge; null deletes: {car}"
    );
    let (_, stored_car) = stored(&app, "urn:ngsi-ld:Car:7").await;
    assert!(
        stored_car.get(format!("{CITY}label")).is_none(),
        "{stored_car}"
    );
    assert!(
        stored_car.get(format!("{CITY}owner")).is_some(),
        "a merge keeps the rest: {stored_car}"
    );

    let (status, body) = odata(
        &app,
        Method::PUT,
        one,
        Some(r#"{"id": "urn:ngsi-ld:Car:7", "speed": 9, "scope": ["/a"]}"#),
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT, "{body}");
    let (_, stored_car) = stored(&app, "urn:ngsi-ld:Car:7").await;
    assert!(
        stored_car.get(format!("{CITY}owner")).is_none()
            && stored_car[format!("{CITY}speed")]["value"] == 9,
        "a replace drops what the body omits: {stored_car}"
    );

    let (status, body) = odata(&app, Method::DELETE, one, None).await;
    assert_eq!(status, StatusCode::NO_CONTENT, "{body}");
    assert_eq!(
        stored(&app, "urn:ngsi-ld:Car:7").await.0,
        StatusCode::NOT_FOUND
    );

    let (status, body) = odata(
        &app,
        Method::POST,
        "Car",
        Some(r#"{"id": "urn:ngsi-ld:Car:8", "scope": ["/a"]}"#),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    let mut req = Request::post(format!("{ROOT}/Car"))
        .header("NGSILD-Tenant", "city")
        .header("Content-Type", "application/json")
        .header("Prefer", "return=minimal");
    let minimal = r#"{"id": "urn:ngsi-ld:Car:9", "scope": ["/a"]}"#;
    req = req.header("Content-Length", minimal.len());
    let Ok(resp) = app
        .clone()
        .oneshot(req.body(Body::from(minimal)).unwrap_or_default())
        .await;
    assert_eq!(resp.status(), StatusCode::NO_CONTENT);
    assert_eq!(
        resp.headers()["OData-EntityId"],
        format!("{ROOT}/Car('urn:ngsi-ld:Car:9')").as_str()
    );
    assert_eq!(resp.headers()["Location"], resp.headers()["OData-EntityId"]);
}

#[tokio::test(flavor = "multi_thread")]
async fn odata_bind_to_hidden_target_is_404_and_writes_nothing() {
    let (_b, app) = city().await.expect("broker");
    let mut answers = Vec::new();
    for (n, person) in [(10, "urn:ngsi-ld:Person:2"), (11, "urn:ngsi-ld:Person:9")] {
        let create = json!({"id": format!("urn:ngsi-ld:Car:{n}"), "scope": ["/a"],
                            "owner@odata.bind": format!("Person('{person}')")})
        .to_string();
        let (status, body) = odata(&app, Method::POST, "Car", Some(&create)).await;
        assert_eq!(status, StatusCode::NOT_FOUND, "{person}: {body}");
        assert_eq!(
            stored(&app, &format!("urn:ngsi-ld:Car:{n}")).await.0,
            StatusCode::NOT_FOUND,
            "{person}: nothing written"
        );
        let merge = json!({"owner@odata.bind": format!("Person('{person}')")}).to_string();
        let (status, merged) = odata(
            &app,
            Method::PATCH,
            "Car('urn:ngsi-ld:Car:1')",
            Some(&merge),
        )
        .await;
        assert_eq!(status, StatusCode::NOT_FOUND, "{person}: {merged}");
        answers.push((body, merged));
    }
    assert_eq!(
        answers[0], answers[1],
        "a hidden target answers like an absent one"
    );
    let (_, car) = stored(&app, "urn:ngsi-ld:Car:1").await;
    assert!(car.get(format!("{CITY}owner")).is_none(), "{car}");
}

#[tokio::test(flavor = "multi_thread")]
async fn odata_upsert_on_absent_and_on_hidden_id_answer_the_same_404() {
    let (_b, app) = city().await.expect("broker");
    let mut answers = Vec::new();
    for (id, set) in [
        ("urn:ngsi-ld:Car:2", "Car"),
        ("urn:ngsi-ld:Car:99", "Car"),
        ("urn:ngsi-ld:Car:1", "Person"),
    ] {
        for (method, body) in [
            (Method::PATCH, Some(r#"{"name": "x"}"#)),
            (Method::PUT, Some(r#"{"name": "x"}"#)),
            (Method::DELETE, None),
        ] {
            let body = if set == "Car" {
                body.map(|_| r#"{"speed": 2}"#)
            } else {
                body
            };
            let (status, answer) =
                odata(&app, method.clone(), &format!("{set}('{id}')"), body).await;
            // A visible Entity of another type is no secret: a merge naming
            // what its own type's rule does not show is 403 before the
            // type selector answers 404 (ADR-0023 rule 7).
            let other_type = set == "Person" && method == Method::PATCH;
            let want = if other_type {
                StatusCode::FORBIDDEN
            } else {
                StatusCode::NOT_FOUND
            };
            assert_eq!(status, want, "{method} {set} {id}: {answer}");
            if !other_type {
                answers.push(answer);
            }
        }
    }
    assert!(answers.windows(2).all(|w| w[0] == w[1]), "{answers:?}");
    assert_eq!(
        stored(&app, "urn:ngsi-ld:Car:99").await.0,
        StatusCode::NOT_FOUND,
        "no upsert (W7)"
    );
    for id in ["urn:ngsi-ld:Car:1", "urn:ngsi-ld:Car:2"] {
        let (_, car) = stored(&app, id).await;
        assert_eq!(
            car[format!("{CITY}speed")]["value"],
            1.0,
            "{id} unchanged: {car}"
        );
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn odata_deep_insert_is_501() {
    let (_b, app) = city().await.expect("broker");
    for body in [
        r#"{"id": "urn:ngsi-ld:Car:20", "scope": ["/a"], "owner": {"id": "urn:ngsi-ld:Person:20"}}"#,
        r#"{"id": "urn:ngsi-ld:Car:20", "scope": ["/a"], "drivers@odata.bind": ["Person('urn:ngsi-ld:Person:1')"]}"#,
    ] {
        let (status, answer) = odata(&app, Method::POST, "Car", Some(body)).await;
        assert_eq!(status, StatusCode::NOT_IMPLEMENTED, "{body}: {answer}");
    }
    let (status, answer) = odata(
        &app,
        Method::PATCH,
        "Car('urn:ngsi-ld:Car:1')",
        Some(r#"{"owner": {"id": "urn:ngsi-ld:Person:20", "name": "new"}}"#),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_IMPLEMENTED, "deep update: {answer}");
    for id in ["urn:ngsi-ld:Car:20", "urn:ngsi-ld:Person:20"] {
        assert_eq!(stored(&app, id).await.0, StatusCode::NOT_FOUND, "{id}");
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn odata_write_refusals_name_no_hidden_thing() {
    let (_b, app) = city().await.expect("broker");
    let one = "Car('urn:ngsi-ld:Car:1')";
    for (method, path, body, want, code) in [
        (
            Method::POST,
            "Car",
            r#"{"speed": 1}"#,
            StatusCode::BAD_REQUEST,
            "MissingKey",
        ),
        (
            Method::POST,
            "Car",
            r#"{"id": "urn:ngsi-ld:Car:1", "scope": ["/a"]}"#,
            StatusCode::CONFLICT,
            "Conflict",
        ),
        (
            Method::POST,
            "Car",
            r#"{"id": "urn:ngsi-ld:Car:2", "scope": ["/a"]}"#,
            StatusCode::CONFLICT,
            "Conflict",
        ),
        (
            Method::POST,
            "Car",
            r#"{"id": "urn:ngsi-ld:Car:21", "createdAt": "2026-01-01T00:00:00Z"}"#,
            StatusCode::BAD_REQUEST,
            "ReadOnly",
        ),
        (
            Method::POST,
            "Car",
            r#"{"id": "urn:ngsi-ld:Car:21", "garage": "urn:ngsi-ld:Garage:1"}"#,
            StatusCode::BAD_REQUEST,
            "ReadOnly",
        ),
        (
            Method::POST,
            "Car",
            r#"{"id": "urn:ngsi-ld:Car:21", "secret": 1}"#,
            StatusCode::BAD_REQUEST,
            "UnknownProperty",
        ),
        (
            Method::PATCH,
            one,
            r#"{"id": "urn:ngsi-ld:Car:2"}"#,
            StatusCode::BAD_REQUEST,
            "InvalidValue",
        ),
        (
            Method::PATCH,
            one,
            r#"{"speed": "fast"}"#,
            StatusCode::BAD_REQUEST,
            "InvalidValue",
        ),
        (
            Method::PATCH,
            one,
            r#"{"garage": null}"#,
            StatusCode::BAD_REQUEST,
            "ReadOnly",
        ),
        (
            Method::PATCH,
            &format!("{one}?$select=speed"),
            "{}",
            StatusCode::NOT_IMPLEMENTED,
            "NotImplemented",
        ),
    ] {
        let body = Some(body).filter(|b| !b.is_empty());
        let (status, answer) = odata(&app, method.clone(), path, body).await;
        assert_eq!(
            (status, answer["error"]["code"].as_str()),
            (want, Some(code)),
            "{method} {path} {body:?}: {answer}"
        );
        let text = answer.to_string();
        assert!(
            !text.contains("/b") && !text.contains("secret") && !text.contains(CITY),
            "{text}"
        );
    }
    let (_, car) = stored(&app, "urn:ngsi-ld:Car:1").await;
    assert_eq!(car[format!("{CITY}speed")]["value"], 1.0, "{car}");
}

#[tokio::test(flavor = "multi_thread")]
async fn odata_a_write_the_view_does_not_offer_is_405_and_the_capabilities_say_so() {
    let (_b, app) = city().await.expect("broker");
    let reads = "/x/views/reads/odata/v4";
    for (method, path, body) in [
        (
            Method::POST,
            "Car",
            Some(r#"{"id": "urn:ngsi-ld:Car:30", "scope": ["/a"]}"#),
        ),
        (
            Method::PATCH,
            "Car('urn:ngsi-ld:Car:1')",
            Some(r#"{"speed": 4}"#),
        ),
        (Method::DELETE, "Car('urn:ngsi-ld:Car:1')", None),
    ] {
        let (status, answer) = send(&app, method.clone(), &format!("{reads}/{path}"), body).await;
        assert_eq!(
            status,
            StatusCode::METHOD_NOT_ALLOWED,
            "{method} {path}: {answer}"
        );
    }
    assert_eq!(
        stored(&app, "urn:ngsi-ld:Car:30").await.0,
        StatusCode::NOT_FOUND
    );
    let flags = |xml: &str| {
        ["Insertable", "Updatable", "Deletable"]
            .map(|f| xml.contains(&format!(r#"<PropertyValue Property="{f}" Bool="true"/>"#)))
    };
    let (_, offered) = send(&app, Method::GET, &format!("{ROOT}/$metadata"), None).await;
    let (_, read_only) = send(&app, Method::GET, &format!("{reads}/$metadata"), None).await;
    let (offered, read_only) = (
        offered.as_str().unwrap_or_default().to_owned(),
        read_only.as_str().unwrap_or_default().to_owned(),
    );
    assert_eq!(flags(&offered), [true; 3], "{offered}");
    assert_eq!(flags(&read_only), [false; 3], "{read_only}");
    let compact: String = offered.split_whitespace().collect();
    assert!(
        compact.contains(r#"<PropertyValueProperty="NonInsertableProperties"><Collection><PropertyPath>garage</PropertyPath></Collection>"#),
        "the R5 member is named: {offered}"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn odata_a_conditional_write_or_one_answered_as_xml_is_refused() {
    let (_b, app) = city().await.expect("broker");
    // Car:1 in view, Car:2 hidden, Car:99 absent: one answer each (O18).
    for (header, value, want, id) in ["1", "2", "99"].into_iter().flat_map(|n| {
        [
            ("If-Match", "*", StatusCode::NOT_IMPLEMENTED, n),
            ("If-None-Match", "*", StatusCode::NOT_IMPLEMENTED, n),
            ("Accept", "application/xml", StatusCode::NOT_ACCEPTABLE, n),
        ]
    }) {
        let req = Request::patch(format!("{ROOT}/Car('urn:ngsi-ld:Car:{id}')"))
            .header("NGSILD-Tenant", "city")
            .header("Content-Type", "application/json")
            .header("Content-Length", 13)
            .header(header, value)
            .body(Body::from(r#"{"speed": 7}"#.to_owned() + " "))
            .unwrap_or_default();
        let Ok(resp) = app.clone().oneshot(req).await;
        assert_eq!(resp.status(), want, "Car:{id} {header}: {value}");
    }
    for (method, path) in [
        (Method::POST, "Car('urn:ngsi-ld:Car:1')"),
        (Method::DELETE, "Car"),
    ] {
        let (status, answer) = odata(&app, method.clone(), path, Some("{}")).await;
        assert_eq!(
            status,
            StatusCode::METHOD_NOT_ALLOWED,
            "{method} {path}: {answer}"
        );
    }
    let (_, car) = stored(&app, "urn:ngsi-ld:Car:1").await;
    assert_eq!(
        car[format!("{CITY}speed")]["value"],
        1.0,
        "nothing written: {car}"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn odata_each_member_is_written_in_its_own_ngsi_ld_form() {
    let (_b, app) = city().await.expect("broker");
    let point = json!({"type": "Point", "coordinates": [13.4, 52.5]});
    let created = json!({
        "id": "urn:ngsi-ld:Car:40", "scope": ["/a"], "label": null,
        "location": point, "notes": {"tyres": ["a", "b"]}, "colour": "red"
    })
    .to_string();
    let (status, body) = odata(&app, Method::POST, "Car", Some(&created)).await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    let (_, car) = stored(&app, "urn:ngsi-ld:Car:40").await;
    let attr = |name: &str| {
        car.get(format!("{CITY}{name}"))
            .cloned()
            .unwrap_or(Value::Null)
    };
    assert_eq!(attr("location")["type"], "GeoProperty", "{car}");
    assert_eq!(
        attr("location")["value"]["coordinates"],
        json!([13.4, 52.5]),
        "{car}"
    );
    assert_eq!(
        (&attr("notes")["type"], &attr("notes")["json"]),
        (&json!("JsonProperty"), &json!({"tyres": ["a", "b"]})),
        "{car}"
    );
    assert_eq!(attr("colour")["type"], "VocabProperty", "{car}");
    assert!(
        attr("colour")["vocab"]
            .as_str()
            .is_some_and(|v| v.ends_with("red")),
        "{car}"
    );
    assert_eq!(
        attr("label"),
        Value::Null,
        "a null in a create is an absent member: {car}"
    );
    let (status, body) = odata(
        &app,
        Method::PATCH,
        "Car('urn:ngsi-ld:Car:40')",
        Some(r#"{"location": null, "notes": null, "colour": null}"#),
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT, "{body}");
    let (_, car) = stored(&app, "urn:ngsi-ld:Car:40").await;
    for name in ["location", "notes", "colour"] {
        assert!(
            car.get(format!("{CITY}{name}")).is_none(),
            "the NGSI-LD Null deletes {name}: {car}"
        );
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn odata_writes_one_property_in_its_own_kind() {
    let (_b, app) = city().await.expect("broker");
    let one = "Car('urn:ngsi-ld:Car:1')";
    let attr = |car: &Value, name: &str| {
        car.get(format!("{CITY}{name}"))
            .cloned()
            .unwrap_or(Value::Null)
    };
    // The attribute writes change what the Entity holds; a merge adds.
    let (status, answer) = odata(
        &app,
        Method::PATCH,
        one,
        Some(r#"{"tags": ["a"], "colour": "red"}"#),
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT, "{answer}");
    for (method, path, body) in [
        (Method::PUT, "speed", r#"{"value": 8}"#),
        (Method::PATCH, "label", r#"{"value": "two"}"#),
        (Method::PATCH, "tags", r#"{"value": ["x", "y"]}"#),
        (Method::DELETE, "colour/$value", ""),
    ] {
        let body = Some(body).filter(|b| !b.is_empty());
        let (status, answer) = odata(&app, method.clone(), &format!("{one}/{path}"), body).await;
        assert_eq!(status, StatusCode::NO_CONTENT, "{method} {path}: {answer}");
    }
    let (_, car) = stored(&app, "urn:ngsi-ld:Car:1").await;
    assert_eq!(
        attr(&car, "speed"),
        json!({"type": "Property", "value": 8}),
        "{car}"
    );
    assert_eq!(attr(&car, "label")["value"], "two", "{car}");
    assert_eq!(
        (
            &attr(&car, "tags")["type"],
            &attr(&car, "tags")["valueList"]
        ),
        (&json!("ListProperty"), &json!(["x", "y"])),
        "a ListProperty keeps its kind: {car}"
    );
    // `null` is the NGSI-LD Null: the partial update removes the value.
    let (status, answer) = odata(
        &app,
        Method::PATCH,
        &format!("{one}/speed"),
        Some(r#"{"value": null}"#),
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT, "{answer}");
    let (_, car) = stored(&app, "urn:ngsi-ld:Car:1").await;
    assert_eq!(attr(&car, "speed"), Value::Null, "{car}");
    let (status, answer) = odata(&app, Method::DELETE, &format!("{one}/label"), None).await;
    assert_eq!(status, StatusCode::NO_CONTENT, "{answer}");
    let (_, car) = stored(&app, "urn:ngsi-ld:Car:1").await;
    assert_eq!(attr(&car, "label"), Value::Null, "{car}");
    let (status, answer) = odata(
        &app,
        Method::PUT,
        &format!("{one}/label"),
        Some(r#"{"value": "back"}"#),
    )
    .await;
    assert_eq!(
        status,
        StatusCode::NOT_FOUND,
        "Replace Attribute sets only what the Entity holds; a merge adds it: {answer}"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn odata_a_property_no_write_may_name_is_refused_before_any_operation() {
    let (_b, app) = city().await.expect("broker");
    let one = "Car('urn:ngsi-ld:Car:1')";
    for (method, path, body, want) in [
        (
            Method::PUT,
            "garage",
            Some(r#"{"value": "urn:ngsi-ld:Garage:1"}"#),
            StatusCode::BAD_REQUEST,
        ),
        (Method::DELETE, "garage", None, StatusCode::BAD_REQUEST),
        (
            Method::PUT,
            "createdAt",
            Some(r#"{"value": "2026-01-01T00:00:00Z"}"#),
            StatusCode::BAD_REQUEST,
        ),
        (Method::DELETE, "id", None, StatusCode::BAD_REQUEST),
        (
            Method::PUT,
            "speed",
            Some(r#"{"value": "fast"}"#),
            StatusCode::BAD_REQUEST,
        ),
        (
            Method::PUT,
            "speed/$value",
            Some("8"),
            StatusCode::NOT_IMPLEMENTED,
        ),
        (
            Method::PATCH,
            "speed/$value",
            Some("8"),
            StatusCode::NOT_IMPLEMENTED,
        ),
    ] {
        let (status, answer) = odata(&app, method.clone(), &format!("{one}/{path}"), body).await;
        assert_eq!(status, want, "{method} {path}: {answer}");
    }
    let mut answers = Vec::new();
    for id in ["urn:ngsi-ld:Car:2", "urn:ngsi-ld:Car:99"] {
        for (method, body) in [
            (Method::PUT, Some(r#"{"value": 3}"#)),
            (Method::DELETE, None),
        ] {
            let (status, answer) =
                odata(&app, method.clone(), &format!("Car('{id}')/speed"), body).await;
            assert_eq!(status, StatusCode::NOT_FOUND, "{method} {id}: {answer}");
            answers.push(answer);
        }
    }
    assert!(answers.windows(2).all(|w| w[0] == w[1]), "{answers:?}");
    let (status, answer) = send(
        &app,
        Method::PUT,
        &format!("/x/views/reads/odata/v4/{one}/speed"),
        Some(r#"{"value": 3}"#),
    )
    .await;
    assert_eq!(status, StatusCode::METHOD_NOT_ALLOWED, "{answer}");
    for id in ["urn:ngsi-ld:Car:1", "urn:ngsi-ld:Car:2"] {
        let (_, car) = stored(&app, id).await;
        assert_eq!(
            car[format!("{CITY}speed")]["value"],
            1.0,
            "{id} unchanged: {car}"
        );
    }
}
