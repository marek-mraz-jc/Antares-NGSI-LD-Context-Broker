// SPDX-License-Identifier: EUPL-1.2
//! Reads through a view (odata-linkml-mapping.md 5.1 rows Q13, Q29, Q30 and
//! S3): an EntitySet, one Entity by key, `/$count`, `$select`, `$top`,
//! `$skip`, `$count`, each the narrowed answer of one operation, and the
//! forms of later rounds refused before any operation runs.

use antares_ops::test_kit::TestBroker;
use axum::body::Body;
use axum::http::{Method, Request, StatusCode};
use axum::Router;
use http_body_util::BodyExt;
use serde_json::{json, Value};
use tower::ServiceExt;

const CITY: &str = "https://example.org/city/";

const SCHEMA: &str = r#"{
  "id": "https://example.org/city", "name": "city",
  "prefixes": {"city": {"prefix_prefix": "city", "prefix_reference": "https://example.org/city/"}},
  "default_prefix": "city",
  "classes": {"Car": {"slots": ["speed", "secret", "owner", "seen", "tags"]}, "Person": {"slots": ["name"]}},
  "slots": {"speed": {"range": "float"}, "secret": {"range": "integer"},
            "owner": {"range": "Person"}, "seen": {"range": "datetime"},
            "tags": {"annotations": {"ngsi_ld_kind": {"tag": "ngsi_ld_kind", "value": "ListProperty"}}},
            "name": {}}
}"#;

struct Answer {
    status: StatusCode,
    headers: axum::http::HeaderMap,
    body: String,
}

impl Answer {
    fn json(&self) -> Value {
        serde_json::from_str(&self.body).unwrap_or(Value::Null)
    }
}

async fn send(
    app: &Router,
    method: Method,
    uri: &str,
    body: Option<&str>,
    headers: &[(&str, &str)],
) -> Answer {
    let mut req = Request::builder().method(method).uri(uri);
    if let Some(b) = body {
        req = req
            .header("Content-Type", "application/json")
            .header("Content-Length", b.len());
    }
    for (k, v) in headers {
        req = req.header(*k, *v);
    }
    let Ok(resp) = app
        .clone()
        .oneshot(
            req.body(Body::from(body.unwrap_or_default().to_owned()))
                .unwrap_or_default(),
        )
        .await;
    let (status, headers) = (resp.status(), resp.headers().clone());
    let bytes = resp
        .into_body()
        .collect()
        .await
        .map(http_body_util::Collected::to_bytes)
        .unwrap_or_default();
    Answer {
        status,
        headers,
        body: String::from_utf8_lossy(&bytes).into_owned(),
    }
}

fn car(n: u32, scope: &str, speed: f64) -> String {
    json!({
        "id": format!("urn:ngsi-ld:Car:{n}"),
        "type": format!("{CITY}Car"),
        "scope": scope,
        format!("{CITY}speed"): {"type": "Property", "value": speed},
        format!("{CITY}secret"): {"type": "Property", "value": 7},
        format!("{CITY}owner"): {"type": "Relationship", "object": "urn:ngsi-ld:Person:1"},
        format!("{CITY}seen"): {"type": "Property", "value": {"@type": "DateTime", "@value": "2026-10-11T08:00:00Z"}},
        format!("{CITY}tags"): {"type": "ListProperty", "valueList": ["a", "b"]},
    })
    .to_string()
}

async fn broker() -> Result<(TestBroker, Router), String> {
    std::env::set_var("ANTARES_SCHEMA_MAX_BYTES", "65536");
    let b = TestBroker::new().await?;
    let app = b.serve(antares_odata::OData)?;
    let view = json!({
        "id": "cars", "tenant": "city",
        "@context": {"city": CITY},
        "selector": ["city:Car"],
        "scopeQ": "/a",
        "rules": [{"types": ["city:Car"], "attributes": ["city:speed", "city:owner", "city:seen", "city:tags"]}],
        "operations": ["query_entities", "retrieve_entity"]
    });
    let put = send(
        &app,
        Method::PUT,
        "/q/views/cars",
        Some(&view.to_string()),
        &[],
    )
    .await;
    if put.status != StatusCode::CREATED {
        return Err(format!("view: {} {}", put.status, put.body));
    }
    let put = send(&app, Method::PUT, "/q/schemas/city", Some(SCHEMA), &[]).await;
    if put.status != StatusCode::CREATED {
        return Err(format!("schema: {} {}", put.status, put.body));
    }
    for (n, scope, speed) in [
        (1, "/a", 12.5),
        (2, "/a", 30.0),
        (3, "/a", 5.0),
        (4, "/b", 99.0),
    ] {
        let made = send(
            &app,
            Method::POST,
            "/ngsi-ld/v1/entities",
            Some(&car(n, scope, speed)),
            &[("NGSILD-Tenant", "city")],
        )
        .await;
        if made.status != StatusCode::CREATED {
            return Err(format!("car {n}: {} {}", made.status, made.body));
        }
    }
    Ok((b, app))
}

const ROOT: &str = "/x/views/cars/odata/v4";

#[tokio::test(flavor = "multi_thread")]
async fn an_entity_set_is_the_narrowed_query_mapped_through_the_model() {
    let (_b, app) = broker().await.expect("broker");
    let got = send(&app, Method::GET, &format!("{ROOT}/Car"), None, &[]).await;
    assert_eq!(got.status, StatusCode::OK, "{}", got.body);
    assert_eq!(
        got.headers["content-type"],
        "application/json;odata.metadata=minimal"
    );
    let body = got.json();
    assert_eq!(
        body["@odata.context"],
        format!("{ROOT}/$metadata#Car"),
        "{body}"
    );
    let cars = body["value"].as_array().cloned().unwrap_or_default();
    assert_eq!(cars.len(), 3, "the view's scope leaves Car:4 out: {body}");
    let one = cars
        .iter()
        .find(|c| c["id"] == "urn:ngsi-ld:Car:1")
        .cloned()
        .unwrap_or(Value::Null);
    assert_eq!(one["speed"], 12.5, "{one}");
    assert_eq!(one["owner"], "urn:ngsi-ld:Person:1", "R5: the object URI");
    assert_eq!(
        one["seen"], "2026-10-11T08:00:00Z",
        "a typed literal is its value"
    );
    assert_eq!(
        one["tags"],
        json!(["a", "b"]),
        "a ListProperty is a collection"
    );
    assert_eq!(one["_types"], json!(["Car"]));
    assert_eq!(one["scope"], json!(["/a"]));
    assert!(
        !got.body.contains("secret") && !got.body.contains('7'),
        "{}",
        got.body
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn odata_count_is_the_narrowed_count() {
    let (_b, app) = broker().await.expect("broker");
    let got = send(
        &app,
        Method::GET,
        &format!("{ROOT}/Car?$count=true&$top=0"),
        None,
        &[],
    )
    .await;
    assert_eq!(got.status, StatusCode::OK, "{}", got.body);
    assert_eq!(got.json()["@odata.count"], 3, "{}", got.body);
    assert_eq!(got.json()["value"], json!([]));
    let got = send(&app, Method::GET, &format!("{ROOT}/Car/$count"), None, &[]).await;
    assert_eq!((got.status, got.body.as_str()), (StatusCode::OK, "3"));
    assert_eq!(got.headers["content-type"], "text/plain");
}

#[tokio::test(flavor = "multi_thread")]
async fn top_and_skip_page_and_the_next_link_carries_the_rest() {
    let (_b, app) = broker().await.expect("broker");
    let got = send(
        &app,
        Method::GET,
        &format!("{ROOT}/Car?$top=2"),
        None,
        &[("Prefer", "odata.maxpagesize=1")],
    )
    .await;
    let body = got.json();
    assert_eq!(body["value"].as_array().map(Vec::len), Some(1), "{body}");
    assert_eq!(
        body["@odata.nextLink"],
        format!("{ROOT}/Car?$top=1&$skip=1"),
        "{body}"
    );
    let got = send(
        &app,
        Method::GET,
        &format!("{ROOT}/Car?$top=1&$skip=1"),
        None,
        &[],
    )
    .await;
    let body = got.json();
    assert_eq!(body["value"].as_array().map(Vec::len), Some(1), "{body}");
    assert!(
        body.get("@odata.nextLink").is_none(),
        "$top is used up: {body}"
    );
    let got = send(&app, Method::GET, &format!("{ROOT}/Car?$skip=2"), None, &[]).await;
    let body = got.json();
    assert_eq!(body["value"].as_array().map(Vec::len), Some(1), "{body}");
    assert!(
        body.get("@odata.nextLink").is_none(),
        "the last page: {body}"
    );
    let got = send(
        &app,
        Method::GET,
        &format!("{ROOT}/Car"),
        None,
        &[("Prefer", "odata.maxpagesize=2")],
    )
    .await;
    assert_eq!(
        got.json()["@odata.nextLink"],
        format!("{ROOT}/Car?$skip=2"),
        "{}",
        got.body
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn one_entity_by_key_and_hidden_answers_like_absent() {
    let (_b, app) = broker().await.expect("broker");
    for uri in [
        format!("{ROOT}/Car('urn:ngsi-ld:Car:2')"),
        format!("{ROOT}/Car(id='urn:ngsi-ld:Car:2')"),
        format!("{ROOT}/Car(@k)?@k='urn:ngsi-ld:Car:2'"),
    ] {
        let got = send(
            &app,
            Method::GET,
            &uri,
            None,
            &[("OData-MaxVersion", "4.01")],
        )
        .await;
        assert_eq!(got.status, StatusCode::OK, "{uri}: {}", got.body);
        let body = got.json();
        assert_eq!(
            body["@context"],
            format!("{ROOT}/$metadata#Car/$entity"),
            "{body}"
        );
        assert_eq!(body["speed"], 30.0, "{body}");
    }
    let absent = send(
        &app,
        Method::GET,
        &format!("{ROOT}/Car('urn:ngsi-ld:Car:9')"),
        None,
        &[],
    )
    .await;
    let hidden = send(
        &app,
        Method::GET,
        &format!("{ROOT}/Car('urn:ngsi-ld:Car:4')"),
        None,
        &[],
    )
    .await;
    assert_eq!(absent.status, StatusCode::NOT_FOUND, "{}", absent.body);
    assert_eq!(
        (hidden.status, hidden.body),
        (absent.status, absent.body),
        "out of the view = absent"
    );
    let other_key = send(&app, Method::GET, &format!("{ROOT}/Car(id=1)"), None, &[]).await;
    assert_eq!(
        other_key.status,
        StatusCode::NOT_FOUND,
        "{}",
        other_key.body
    );
    let not_a_uri = send(
        &app,
        Method::GET,
        &format!("{ROOT}/Car('not%20a%20uri')"),
        None,
        &[],
    )
    .await;
    assert_eq!(
        not_a_uri.status,
        StatusCode::BAD_REQUEST,
        "{}",
        not_a_uri.body
    );
    assert_eq!(
        not_a_uri.json()["error"]["message"],
        "the request was refused"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn select_projects_and_asks_only_for_what_it_names() {
    let (_b, app) = broker().await.expect("broker");
    let got = send(
        &app,
        Method::GET,
        &format!("{ROOT}/Car('urn:ngsi-ld:Car:1')?$select=speed,createdAt"),
        None,
        &[],
    )
    .await;
    assert_eq!(got.status, StatusCode::OK, "{}", got.body);
    let body = got.json();
    assert_eq!(
        body["@odata.context"],
        format!("{ROOT}/$metadata#Car(speed,createdAt)/$entity"),
        "{body}"
    );
    let mut names: Vec<&str> = body
        .as_object()
        .into_iter()
        .flatten()
        .map(|(k, _)| k.as_str())
        .filter(|k| !k.starts_with('@'))
        .collect();
    names.sort_unstable();
    assert_eq!(names, vec!["createdAt", "id", "speed"], "{body}");
    assert_eq!(body["speed"], 12.5, "{body}");
    assert!(body["createdAt"].is_string(), "{body}");
    let got = send(
        &app,
        Method::GET,
        &format!("{ROOT}/Car?$select=*"),
        None,
        &[],
    )
    .await;
    assert!(got.json()["value"][0].get("tags").is_some(), "{}", got.body);
    let got = send(
        &app,
        Method::GET,
        &format!("{ROOT}/Car?$select=scope,_types"),
        None,
        &[],
    )
    .await;
    assert_eq!(
        got.json()["value"][0]["scope"],
        json!(["/a"]),
        "{}",
        got.body
    );
    let got = send(
        &app,
        Method::GET,
        &format!("{ROOT}/Car?$select=owner,tags"),
        None,
        &[],
    )
    .await;
    let first = &got.json()["value"][0];
    assert_eq!(first["owner"], "urn:ngsi-ld:Person:1", "{}", got.body);
    assert_eq!(first["tags"], json!(["a", "b"]), "{}", got.body);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_hidden_or_unknown_name_is_the_same_400() {
    let (_b, app) = broker().await.expect("broker");
    let hidden = send(
        &app,
        Method::GET,
        &format!("{ROOT}/Car?$select=secret"),
        None,
        &[],
    )
    .await;
    let unknown = send(
        &app,
        Method::GET,
        &format!("{ROOT}/Car?$select=nosuch"),
        None,
        &[],
    )
    .await;
    assert_eq!(hidden.status, StatusCode::BAD_REQUEST, "{}", hidden.body);
    assert_eq!(
        hidden.json()["error"]["code"],
        unknown.json()["error"]["code"]
    );
    assert!(!hidden.body.contains("secret"), "{}", hidden.body);
    let set = send(&app, Method::GET, &format!("{ROOT}/Person"), None, &[]).await;
    assert_eq!(
        set.status,
        StatusCode::NOT_FOUND,
        "a hidden type has no set: {}",
        set.body
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn the_forms_of_later_rounds_are_501_and_writes_405() {
    let (_b, app) = broker().await.expect("broker");
    for uri in [
        "Car?$filter=speed%20gt%201",
        "Car?$orderby=speed",
        "Car?$expand=*",
        "Car?$skiptoken=x",
        "Car?$schemaversion=2",
        "Car('urn:ngsi-ld:Car:1')/speed",
        "$entity?$id=Car('urn:ngsi-ld:Car:1')",
        "$batch",
    ] {
        let got = send(&app, Method::GET, &format!("{ROOT}/{uri}"), None, &[]).await;
        assert_eq!(
            got.status,
            StatusCode::NOT_IMPLEMENTED,
            "{uri}: {}",
            got.body
        );
        assert_eq!(got.json()["error"]["code"], "NotImplemented", "{uri}");
    }
    let got = send(
        &app,
        Method::GET,
        &format!("{ROOT}/Car?$schemaversion=*"),
        None,
        &[],
    )
    .await;
    assert_eq!(got.status, StatusCode::OK, "{}", got.body);
    let got = send(&app, Method::POST, &format!("{ROOT}/Car"), Some("{}"), &[]).await;
    assert_eq!(got.status, StatusCode::METHOD_NOT_ALLOWED, "{}", got.body);
    assert_eq!(got.headers["allow"], "GET");
    let got = send(
        &app,
        Method::GET,
        &format!("{ROOT}/Car?$format=xml"),
        None,
        &[],
    )
    .await;
    assert_eq!(got.status, StatusCode::NOT_ACCEPTABLE, "{}", got.body);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_view_without_the_read_operations_is_refused_by_its_allowlist() {
    let (_b, app) = broker().await.expect("broker");
    let view = json!({
        "id": "counts", "tenant": "city", "@context": {"city": CITY},
        "selector": ["city:Car"],
        "rules": [{"types": ["city:Car"], "attributes": ["city:speed"]}],
        "operations": ["retrieve_entity_types"]
    });
    let put = send(
        &app,
        Method::PUT,
        "/q/views/counts",
        Some(&view.to_string()),
        &[],
    )
    .await;
    assert_eq!(put.status, StatusCode::CREATED, "{}", put.body);
    let got = send(&app, Method::GET, "/x/views/counts/odata/v4/Car", None, &[]).await;
    assert!(got.status.is_client_error(), "{}: {}", got.status, got.body);
    assert_eq!(
        got.json()["error"]["message"],
        "the request was refused",
        "no broker detail"
    );
}
