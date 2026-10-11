// SPDX-License-Identifier: EUPL-1.2
//! Capabilities annotations (odata-linkml-mapping section 4, C11 to C13,
//! C20; surfaces-threat-model O25): `$metadata` names the members `$filter`
//! and `$orderby` refuse, and the annotations are advisory, never a grant:
//! a request the engine denies is 403 whatever they say.

use antares_ops::policy::{
    Decision, DecisionFuture, NotifyDecision, Operation, PolicyEngine, Subject,
};
use antares_ops::test_kit::TestBroker;
use axum::body::Body;
use axum::http::{Method, Request, StatusCode};
use axum::Router;
use http_body_util::BodyExt;
use serde_json::{json, Value};
use std::sync::Arc;
use tower::ServiceExt;

const ROOT: &str = "/x/views/cars/odata/v4";

const SCHEMA: &str = r#"{
  "id": "https://example.org/city", "name": "city",
  "prefixes": {"city": {"prefix_prefix": "city", "prefix_reference": "https://example.org/city/"}},
  "default_prefix": "city",
  "classes": {"Car": {"slots": ["speed", "tags", "doc", "brand"]}},
  "slots": {"speed": {"range": "float"}, "doc": {}, "brand": {"range": "string"},
            "tags": {"annotations": {"ngsi_ld_kind": {"tag": "ngsi_ld_kind", "value": "ListProperty"}}}}
}"#;

/// Allows everything but the reads of 5.7.1 and 5.7.2 when `deny` is set.
struct Reads {
    deny: bool,
}

impl PolicyEngine for Reads {
    fn name(&self) -> &'static str {
        "reads"
    }
    fn decide<'a>(&'a self, _: &'a Subject, op: &'a Operation<'a>) -> DecisionFuture<'a> {
        let denied = self.deny && matches!(op.clause, "5.7.1" | "5.7.2");
        Box::pin(std::future::ready(if denied {
            Decision::Deny("reads".into())
        } else {
            Decision::Allow
        }))
    }
    fn pre_notify(&self, _: &Subject, _: &Value, _: &mut Value) -> NotifyDecision {
        NotifyDecision::Deliver
    }
}

async fn send(app: &Router, method: Method, uri: &str, body: Option<&str>) -> (StatusCode, String) {
    let mut req = Request::builder().method(method).uri(uri);
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
    (status, String::from_utf8_lossy(&bytes).into_owned())
}

async fn broker(deny: bool) -> Result<(TestBroker, Router), String> {
    std::env::set_var("ANTARES_SCHEMA_MAX_BYTES", "65536");
    let b = TestBroker::new()
        .await?
        .with_policy(Arc::new(Reads { deny }));
    let app = b.serve(antares_odata::OData)?;
    let view = json!({
        "id": "cars", "tenant": "city", "@context": {"city": "https://example.org/city/"},
        "selector": ["city:Car"],
        "rules": [{"types": ["city:Car"], "attributes": ["city:speed", "city:tags", "city:doc", "city:brand"]}],
        "operations": ["query_entities", "retrieve_entity"]
    })
    .to_string();
    for (uri, body) in [
        ("/q/views/cars", view.as_str()),
        ("/q/schemas/city", SCHEMA),
    ] {
        let (status, answer) = send(&app, Method::PUT, uri, Some(body)).await;
        if status != StatusCode::CREATED {
            return Err(format!("{uri}: {status} {answer}"));
        }
    }
    let car = json!({
        "id": "urn:ngsi-ld:Car:1", "type": "https://example.org/city/Car",
        "https://example.org/city/speed": {"type": "Property", "value": 3.0}
    })
    .to_string();
    let req = Request::post("/ngsi-ld/v1/entities")
        .header("NGSILD-Tenant", "city")
        .header("Content-Type", "application/json")
        .header("Content-Length", car.len())
        .body(Body::from(car))
        .map_err(|e| e.to_string())?;
    let Ok(made) = app.clone().oneshot(req).await;
    if made.status() != StatusCode::CREATED {
        return Err(format!("car: {}", made.status()));
    }
    Ok((b, app))
}

/// The `PropertyPath`s of `member` in the annotation `term` of the CSDL XML.
fn paths(xml: &str, term: &str, member: &str) -> Vec<String> {
    let Some(at) = xml.find(&format!(r#"Term="Capabilities.{term}""#)) else {
        return Vec::new();
    };
    let rest = xml.get(at..).unwrap_or_default();
    let annotation = rest.split("</Annotation>").next().unwrap_or_default();
    let Some(from) = annotation.find(&format!(r#"Property="{member}""#)) else {
        return Vec::new();
    };
    let value = annotation
        .get(from..)
        .unwrap_or_default()
        .split("</PropertyValue>")
        .next()
        .unwrap_or_default();
    value
        .split("<PropertyPath>")
        .skip(1)
        .filter_map(|p| p.split("</PropertyPath>").next())
        .map(str::to_owned)
        .collect()
}

#[tokio::test(flavor = "multi_thread")]
async fn the_restrictions_name_what_filter_and_orderby_refuse() {
    let (_b, app) = broker(false).await.expect("broker");
    let (status, xml) = send(&app, Method::GET, &format!("{ROOT}/$metadata"), None).await;
    assert_eq!(status, StatusCode::OK, "{xml}");
    let unfilterable = paths(&xml, "FilterRestrictions", "NonFilterableProperties");
    let unsortable = paths(&xml, "SortRestrictions", "NonSortableProperties");
    assert_eq!(unfilterable, unsortable, "{xml}");
    for name in ["tags", "_types", "scope"] {
        assert!(unfilterable.iter().any(|p| p == name), "{name}: {xml}");
    }
    for (option, served) in [
        ("$orderby", "speed%20desc"),
        ("$filter", "brand%20eq%20'x'"),
    ] {
        let (status, body) = send(
            &app,
            Method::GET,
            &format!("{ROOT}/Car?{option}={served}"),
            None,
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{option}={served}: {body}");
    }
    for name in &unfilterable {
        for uri in [
            format!("{ROOT}/Car?$filter={name}%20eq%20null"),
            format!("{ROOT}/Car?$orderby={name}"),
        ] {
            let (status, body) = send(&app, Method::GET, &uri, None).await;
            assert!(
                status == StatusCode::BAD_REQUEST || status == StatusCode::NOT_IMPLEMENTED,
                "{uri}: annotated as refused, answered {status} {body}"
            );
        }
    }
    for (term, member, value) in [
        ("FilterRestrictions", "MaxLevels", r#"Int="1""#),
        ("ExpandRestrictions", "Expandable", r#"Bool="true""#),
        ("ExpandRestrictions", "MaxLevels", r#"Int="1""#),
        ("UpdateRestrictions", "Upsertable", r#"Bool="false""#),
    ] {
        let at = xml
            .find(&format!(r#"Term="Capabilities.{term}""#))
            .and_then(|at| xml.get(at..))
            .and_then(|rest| rest.split("</Annotation>").next())
            .unwrap_or_default();
        assert!(
            at.contains(&format!(r#"<PropertyValue Property="{member}" {value}/>"#)),
            "{term}/{member}: {at}"
        );
    }
    assert!(
        xml.contains(r#"<Annotation Term="Capabilities.QuerySegmentSupported" Bool="true"/>"#),
        "C20: POST /$query is served: {xml}"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn odata_capabilities_true_but_engine_denies_is_403() {
    let (_b, app) = broker(true).await.expect("broker");
    let (status, xml) = send(&app, Method::GET, &format!("{ROOT}/$metadata"), None).await;
    assert_eq!(status, StatusCode::OK, "{xml}");
    for claim in [
        r#"<PropertyValue Property="Countable" Bool="true"/>"#,
        r#"<PropertyValue Property="Filterable" Bool="true"/>"#,
        r#"<PropertyValue Property="Sortable" Bool="true"/>"#,
    ] {
        assert!(xml.contains(claim), "{claim}: {xml}");
    }
    for path in [
        "Car",
        "Car/$count",
        "Car?$filter=speed%20gt%201",
        "Car?$orderby=speed",
        "Car?$count=true",
    ] {
        let (status, body) = send(&app, Method::GET, &format!("{ROOT}/{path}"), None).await;
        assert_eq!(status, StatusCode::FORBIDDEN, "{path}: {body}");
        assert!(!body.contains("urn:ngsi-ld:Car:1"), "{path}: {body}");
    }
}
