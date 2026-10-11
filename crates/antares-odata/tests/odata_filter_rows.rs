// SPDX-License-Identifier: EUPL-1.2
//! `$filter` rows F21, F22, F29, F31 and F32 (odata-linkml-mapping.md 5.2)
//! through a view: each answers the Entities the NGSI-LD `q` or `geoQ` it
//! maps to selects, and a hidden Attribute stays the 400 an unknown name is
//! (V1, O1).

use antares_ops::test_kit::TestBroker;
use axum::body::Body;
use axum::http::{Method, Request, StatusCode};
use axum::Router;
use http_body_util::BodyExt;
use serde_json::{json, Value};
use tower::ServiceExt;

const CITY: &str = "https://example.org/city/";
const ROOT: &str = "/x/views/cars/odata/v4";

const SCHEMA: &str = r#"{
  "id": "https://example.org/city", "name": "city",
  "prefixes": {"city": {"prefix_prefix": "city", "prefix_reference": "https://example.org/city/"}},
  "default_prefix": "city",
  "classes": {"Car": {"slots": ["speed", "tags", "label", "status", "location", "secret"]}},
  "slots": {"speed": {"range": "float"}, "secret": {"range": "float"},
            "tags": {"annotations": {"ngsi_ld_kind": {"tag": "ngsi_ld_kind", "value": "ListProperty"}}},
            "label": {"annotations": {"ngsi_ld_kind": {"tag": "ngsi_ld_kind", "value": "LanguageProperty"}}},
            "status": {"annotations": {"ngsi_ld_kind": {"tag": "ngsi_ld_kind", "value": "VocabProperty"}}},
            "location": {"annotations": {"ngsi_ld_kind": {"tag": "ngsi_ld_kind", "value": "GeoProperty"}}}}
}"#;

async fn send(app: &Router, method: Method, uri: &str, body: Option<&str>) -> (StatusCode, Value) {
    let mut req = Request::builder()
        .method(method)
        .uri(uri)
        .header("NGSILD-Tenant", "city");
    if let Some(b) = body {
        req = req
            .header("Content-Type", "application/json")
            .header("Content-Length", b.len());
    }
    let Ok(resp) = app
        .clone()
        .oneshot(
            req.body(Body::from(body.unwrap_or_default().to_owned()))
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

fn car(n: u32, observed: &str, tags: &[&str], label: &Value, status: &str, at: [f64; 2]) -> Value {
    json!({
        "id": format!("urn:ngsi-ld:Car:{n}"),
        "type": format!("{CITY}Car"),
        format!("{CITY}speed"): {"type": "Property", "value": 10.0 * f64::from(n), "observedAt": observed},
        format!("{CITY}secret"): {"type": "Property", "value": 1.0, "observedAt": observed},
        format!("{CITY}tags"): {"type": "ListProperty", "valueList": tags},
        format!("{CITY}label"): {"type": "LanguageProperty", "languageMap": label},
        format!("{CITY}status"): {"type": "VocabProperty", "vocab": format!("{CITY}{status}")},
        format!("{CITY}location"): {"type": "GeoProperty", "value": {"type": "Point", "coordinates": at}},
    })
}

async fn broker() -> Result<(TestBroker, Router), String> {
    std::env::set_var(antares_ops::views::SCHEMA_MAX_ENV, "65536");
    let b = TestBroker::new().await?;
    let app = b.serve(antares_odata::OData)?;
    let view = json!({
        "id": "cars", "tenant": "city",
        "@context": {"city": CITY},
        "selector": ["city:Car"],
        "rules": [{"types": ["city:Car"],
                   "attributes": ["city:speed", "city:tags", "city:label", "city:status", "city:location"]}],
        "operations": ["query_entities", "retrieve_entity"]
    });
    let (status, body) = send(&app, Method::PUT, "/q/views/cars", Some(&view.to_string())).await;
    if status != StatusCode::CREATED {
        return Err(format!("view: {status} {body}"));
    }
    let (status, body) = send(&app, Method::PUT, "/q/schemas/city", Some(SCHEMA)).await;
    if status != StatusCode::CREATED {
        return Err(format!("schema: {status} {body}"));
    }
    for entity in [
        car(
            1,
            "2026-05-01T08:00:00Z",
            &["a", "b"],
            &json!({"en": "red", "cs": "červená"}),
            "Active",
            [0.5, 0.5],
        ),
        car(
            2,
            "2026-05-02T08:00:00Z",
            &["c"],
            &json!({"en": "blue"}),
            "Parked",
            [5.0, 5.0],
        ),
    ] {
        let (status, body) = send(
            &app,
            Method::POST,
            "/ngsi-ld/v1/entities",
            Some(&entity.to_string()),
        )
        .await;
        if status != StatusCode::CREATED {
            return Err(format!(
                "{}: {status} {body}",
                entity.get("id").unwrap_or(&Value::Null)
            ));
        }
    }
    Ok((b, app))
}

fn ids(answer: &Value) -> Vec<&str> {
    let mut ids: Vec<&str> = answer
        .get("value")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|e| e.get("id").and_then(Value::as_str))
        .collect();
    ids.sort_unstable();
    ids
}

#[tokio::test(flavor = "multi_thread")]
async fn the_mapped_rows_select_what_their_ngsi_ld_query_selects() {
    let (_b, app) = broker().await.expect("broker");
    let polygon = "geography'SRID=4326;Polygon((0%200,1%200,1%201,0%201,0%200))'";
    let vocab = |name: &str| {
        let iri = format!("{CITY}{name}")
            .replace(':', "%3A")
            .replace('/', "%2F");
        format!("status%20eq%20'{iri}'")
    };
    for (row, filter, want) in [
        (
            "F21",
            "speed/@NgsiLd.observedAt%20ge%202026-05-01T12:00:00Z".to_owned(),
            vec!["urn:ngsi-ld:Car:2"],
        ),
        (
            "F21",
            "speed/@NgsiLd.observedAt%20lt%202026-05-01T12:00:00Z".to_owned(),
            vec!["urn:ngsi-ld:Car:1"],
        ),
        (
            "F22",
            "tags/any(x:x%20eq%20'a')".to_owned(),
            vec!["urn:ngsi-ld:Car:1"],
        ),
        (
            "F22",
            "tags/any(x:x%20in%20('c','z'))".to_owned(),
            vec!["urn:ngsi-ld:Car:2"],
        ),
        ("F22", "tags/any(x:x%20eq%20'z')".to_owned(), vec![]),
        (
            "F29",
            format!("geo.intersects(location,{polygon})"),
            vec!["urn:ngsi-ld:Car:1"],
        ),
        (
            "F29",
            format!("geo.intersects(location,{polygon})%20and%20speed%20gt%2015"),
            vec![],
        ),
        (
            "F31",
            "label%20eq%20'%C4%8Derven%C3%A1'".to_owned(),
            vec!["urn:ngsi-ld:Car:1"],
        ),
        (
            "F31",
            "label%20eq%20'blue'".to_owned(),
            vec!["urn:ngsi-ld:Car:2"],
        ),
        ("F32", vocab("Active"), vec!["urn:ngsi-ld:Car:1"]),
        ("F32", vocab("Parked"), vec!["urn:ngsi-ld:Car:2"]),
    ] {
        let (status, answer) = send(
            &app,
            Method::GET,
            &format!("{ROOT}/Car?$filter={filter}"),
            None,
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{row} {filter}: {answer}");
        assert_eq!(ids(&answer), want, "{row} {filter}: {answer}");
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn a_vocab_value_matches_as_the_read_shows_it() {
    let (_b, app) = broker().await.expect("broker");
    let (_, one) = send(
        &app,
        Method::GET,
        &format!("{ROOT}/Car('urn:ngsi-ld:Car:2')"),
        None,
    )
    .await;
    // F31 is not here: the read shows a languageMap only by its `@none`
    // entry, the filter matches any language (5.2 F31)
    for name in ["status"] {
        let shown = one.get(name).and_then(Value::as_str).unwrap_or_default();
        assert!(!shown.is_empty(), "{name}: {one}");
        let literal: String = shown
            .bytes()
            .map(|b| match b {
                b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' => {
                    (b as char).to_string()
                }
                _ => format!("%{b:02X}"),
            })
            .collect();
        let (status, answer) = send(
            &app,
            Method::GET,
            &format!("{ROOT}/Car?$filter={name}%20eq%20'{literal}'"),
            None,
        )
        .await;
        assert_eq!(
            (status, ids(&answer)),
            (StatusCode::OK, vec!["urn:ngsi-ld:Car:2"]),
            "{name} shown as {shown}: {answer}"
        );
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn a_hidden_attributes_observed_at_is_the_400_of_an_unknown_name() {
    let (_b, app) = broker().await.expect("broker");
    let at = |name: &str| {
        format!("{ROOT}/Car?$filter={name}/@NgsiLd.observedAt%20ge%202026-05-01T12:00:00Z")
    };
    let (hidden_status, hidden) = send(&app, Method::GET, &at("secret"), None).await;
    let (unknown_status, unknown) = send(&app, Method::GET, &at("nothing"), None).await;
    assert_eq!(hidden_status, StatusCode::BAD_REQUEST, "{hidden}");
    assert_eq!(
        (hidden_status, &hidden),
        (unknown_status, &unknown),
        "V1, O1"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn the_forms_these_rows_do_not_cover_stay_501() {
    let (_b, app) = broker().await.expect("broker");
    let polygon = "geography'SRID=4326;Polygon((0%200,1%200,1%201,0%201,0%200))'";
    for filter in [
        format!("geo.intersects(location,{polygon})%20or%20speed%20gt%2015"),
        "label%20ne%20'blue'".to_owned(),
        "tags/any(x:x%20ne%20'a')".to_owned(),
        "tags/all(x:x%20eq%20'a')".to_owned(),
    ] {
        let (status, answer) = send(
            &app,
            Method::GET,
            &format!("{ROOT}/Car?$filter={filter}"),
            None,
        )
        .await;
        assert_eq!(status, StatusCode::NOT_IMPLEMENTED, "{filter}: {answer}");
    }
}
