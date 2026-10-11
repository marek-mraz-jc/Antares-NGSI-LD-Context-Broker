// SPDX-License-Identifier: EUPL-1.2
//! `/x/views/{view}/linkml` against a broker over the built-in store
//! (T-3511, threats LK1 to LK8): what each path serves, what it never
//! names, and how it refuses.

use antares_ops::policy::{
    Decision, DecisionFuture, Filter, NotifyDecision, Operation, PolicyEngine, Subject,
};
use antares_ops::test_kit::TestBroker;
use axum::body::Body;
use axum::http::{header, Request, StatusCode};
use axum::Router;
use http_body_util::BodyExt;
use serde_json::{json, Value};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use tower::ServiceExt;

/// What a hidden name looks like: every test checks no answer carries one.
const HIDDEN: [&str; 4] = ["Person", "owner_secret", "Secret", "hidden.e.org"];

fn schema() -> Value {
    json!({
      "id": "https://e.org/city", "name": "city", "version": "2.1.0",
      "prefixes": {
        "city": {"prefix_prefix": "city", "prefix_reference": "https://e.org/city/"},
        "hidden": {"prefix_prefix": "hidden", "prefix_reference": "https://hidden.e.org/"}
      },
      "default_prefix": "city",
      "classes": {
        "Car": {"slots": ["speed", "owner", "fuel", "owner_secret"],
                "description": "A car."},
        "Person": {"class_uri": "hidden:Person", "slots": ["name"]}
      },
      "slots": {
        "speed": {"range": "float", "required": true},
        "owner": {"range": "Person"},
        "fuel": {"range": "Fuel"},
        "owner_secret": {"range": "Secret"},
        "name": {}
      },
      "enums": {"Fuel": {"permissible_values": {"diesel": {}}},
                "Secret": {"permissible_values": {"x": {}}}}
    })
}

fn view(operations: &[&str]) -> Value {
    json!({
      "id": "cars", "tenant": "city",
      "@context": {"city": "https://e.org/city/"},
      "selector": ["city:Car"],
      "rules": [{"types": ["city:Car"], "attributes": ["city:speed", "city:owner", "city:fuel"]}],
      "operations": operations
    })
}

/// The schema bound every broker here runs with.
const SCHEMA_MAX: usize = 64 * 1024;

const READS: [&str; 3] = [
    "retrieve_entity_type_details",
    "retrieve_entity_type_info",
    "retrieve_attribute_info",
];

async fn send(
    app: &Router,
    method: &str,
    uri: &str,
    headers: &[(&str, &str)],
    body: Option<&Value>,
) -> (StatusCode, axum::http::HeaderMap, String) {
    let mut req = Request::builder().method(method).uri(uri);
    let bytes = body.map(Value::to_string).unwrap_or_default();
    if body.is_some() {
        req = req
            .header("Content-Type", "application/json")
            .header("Content-Length", bytes.len());
    }
    for (k, v) in headers {
        req = req.header(*k, *v);
    }
    let Ok(resp) = app
        .clone()
        .oneshot(req.body(Body::from(bytes)).unwrap_or_default())
        .await;
    let (parts, body) = resp.into_parts();
    let body = body
        .collect()
        .await
        .map(http_body_util::Collected::to_bytes)
        .unwrap_or_default();
    (
        parts.status,
        parts.headers,
        String::from_utf8_lossy(&body).into_owned(),
    )
}

async fn get(app: &Router, path: &str) -> (StatusCode, axum::http::HeaderMap, String) {
    send(
        app,
        "GET",
        &format!("/x/views/cars/linkml/{path}"),
        &[],
        None,
    )
    .await
}

/// A broker asking `engine` (none when `None`) with view `doc` loaded and
/// `schema` stored for its Tenant (`/q/schemas/city`, T-3628).
async fn broker(
    engine: Option<Arc<dyn PolicyEngine>>,
    doc: &Value,
    schema: Option<&Value>,
) -> Result<Router, String> {
    // Read once when the broker is wired; every test here sets the same value.
    std::env::set_var(antares_ops::views::SCHEMA_MAX_ENV, SCHEMA_MAX.to_string());
    let mut broker = TestBroker::new().await?;
    if let Some(engine) = engine {
        broker = broker.with_policy(engine);
    }
    let app = broker.serve(antares_linkml::Linkml)?;
    let (status, _, body) = send(&app, "PUT", "/q/views/cars", &[], Some(doc)).await;
    if !status.is_success() {
        return Err(format!("the view does not load: {status} {body}"));
    }
    if let Some(schema) = schema {
        let (status, _, body) = send(&app, "PUT", "/q/schemas/city", &[], Some(schema)).await;
        if !status.is_success() {
            return Err(format!("the schema does not load: {status} {body}"));
        }
    }
    Ok(app)
}

fn assert_no_hidden_name(what: &str, body: &str) {
    for name in HIDDEN {
        assert!(!body.contains(name), "{what} names {name}: {body}");
    }
}

/// Narrows clause 5.7.6 by omitting `speed` while `narrow` is set.
struct OmitSpeed(AtomicBool);

impl PolicyEngine for OmitSpeed {
    fn name(&self) -> &'static str {
        "omit-speed"
    }
    fn decide<'a>(&'a self, _: &'a Subject, _: &'a Operation<'a>) -> DecisionFuture<'a> {
        let d = if self.0.load(Ordering::SeqCst) {
            Decision::Filter(Filter {
                omit: vec!["https://e.org/city/speed".into()],
                ..Filter::default()
            })
        } else {
            Decision::Allow
        };
        Box::pin(std::future::ready(d))
    }
    fn pre_notify(&self, _: &Subject, _: &Value, _: &mut Value) -> NotifyDecision {
        NotifyDecision::Deliver
    }
}

/// Denies every discovery clause with a reason that names nothing hidden.
struct DenyAll;

impl PolicyEngine for DenyAll {
    fn name(&self) -> &'static str {
        "deny-all"
    }
    fn decide<'a>(&'a self, _: &'a Subject, _: &'a Operation<'a>) -> DecisionFuture<'a> {
        Box::pin(std::future::ready(Decision::Deny("not for you".into())))
    }
    fn pre_notify(&self, _: &Subject, _: &Value, _: &mut Value) -> NotifyDecision {
        NotifyDecision::Deliver
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn linkml_prefixes_and_context_hold_only_visible_terms() {
    let app = broker(None, &view(&READS), Some(&schema()))
        .await
        .expect("a broker");
    let (s, h, yaml) = get(&app, "schema/v2/model.linkml.yaml").await;
    assert_eq!(s, StatusCode::OK, "{yaml}");
    assert_eq!(h[header::CONTENT_TYPE], "text/yaml");
    assert!(yaml.contains("\"https://e.org/city/views/cars\""), "{yaml}");
    assert!(
        yaml.contains("\"speed\":") && yaml.contains("\"Fuel\":"),
        "{yaml}"
    );
    assert_no_hidden_name("the YAML", &yaml);

    let (s, _, context) = get(&app, "schema/v2/context.jsonld").await;
    assert_eq!(s, StatusCode::OK);
    let context: Value = serde_json::from_str(&context).expect("json");
    let mut terms: Vec<&String> = context["@context"]
        .as_object()
        .expect("terms")
        .keys()
        .collect();
    terms.sort();
    assert_eq!(terms, ["Car", "fuel", "owner", "speed"]);

    for doc in [
        "model.schema.json",
        "model.shacl.ttl",
        "model.owl.ttl",
        "model.rdf.ttl",
        "model.md",
        "model",
    ] {
        let (s, _, body) = get(&app, &format!("schema/v2/{doc}")).await;
        assert_eq!(s, StatusCode::OK, "{doc}");
        assert_no_hidden_name(doc, &body);
    }
    let (s, _, index) = get(&app, "schema/index.json").await;
    assert_eq!(s, StatusCode::OK);
    assert_no_hidden_name("the index", &index);
    let index: Value = serde_json::from_str(&index).expect("json");
    assert_eq!(index["models"][0]["types"], json!(["Car"]));
    assert_eq!(index["models"][0]["redacted"], true);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_class_and_a_slot_are_served_and_out_of_view_ones_are_404() {
    let app = broker(None, &view(&READS), Some(&schema()))
        .await
        .expect("a broker");
    let (s, _, car) = get(&app, "classes/Car").await;
    assert_eq!(s, StatusCode::OK, "{car}");
    let car: Value = serde_json::from_str(&car).expect("json");
    let slots: Vec<&str> = car["slots"]
        .as_array()
        .expect("slots")
        .iter()
        .filter_map(|s| s["name"].as_str())
        .collect();
    assert_eq!(slots, ["speed", "owner", "fuel"]);
    assert_eq!(car["iri"], "https://e.org/city/Car");
    let (s, _, speed) = get(&app, "slots/speed").await;
    assert_eq!(s, StatusCode::OK);
    let speed: Value = serde_json::from_str(&speed).expect("json");
    assert_eq!(
        (speed["range"].as_str(), speed["classes"].clone()),
        (Some("float"), json!(["Car"]))
    );

    let (hidden, _, body) = get(&app, "classes/Person").await;
    let (absent, _, other) = get(&app, "classes/Nothing").await;
    assert_eq!(
        (hidden, absent),
        (StatusCode::NOT_FOUND, StatusCode::NOT_FOUND)
    );
    assert_eq!(body, other, "a hidden class answers as an absent one");
    for path in ["slots/owner_secret", "slots/name", "slots/nothing"] {
        assert_eq!(get(&app, path).await.0, StatusCode::NOT_FOUND, "{path}");
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn linkml_errors_carry_no_hidden_name() {
    let app = broker(None, &view(&READS), Some(&schema()))
        .await
        .expect("a broker");
    for path in [
        "classes/Person",
        "slots/owner_secret",
        "schema/v9/model.linkml.yaml",
        "schema/v2/model.qb.ttl",
        "schema/v2/qb",
        "schema/v2/Person",
        "other",
    ] {
        let (s, _, body) = get(&app, path).await;
        assert_eq!(s, StatusCode::NOT_FOUND, "{path}");
        assert_no_hidden_name(path, &body);
    }
    let (s, _, body) = send(
        &app,
        "GET",
        "/x/views/nothing/linkml/schema/index.json",
        &[],
        None,
    )
    .await;
    assert_eq!(s, StatusCode::NOT_FOUND);
    assert_no_hidden_name("an unknown view", &body);

    let denied = broker(Some(Arc::new(DenyAll)), &view(&READS), Some(&schema()))
        .await
        .expect("a broker");
    let (s, _, body) = get(&denied, "classes/Car").await;
    assert_eq!(s, StatusCode::FORBIDDEN, "{body}");
    assert_no_hidden_name("a denial", &body);
}

#[tokio::test(flavor = "multi_thread")]
async fn linkml_is_read_only() {
    let app = broker(None, &view(&READS), Some(&schema()))
        .await
        .expect("a broker");
    for method in ["POST", "PUT", "PATCH", "DELETE"] {
        let (s, h, _) = send(
            &app,
            method,
            "/x/views/cars/linkml/schema/v2/model.linkml.yaml",
            &[],
            Some(&json!({})),
        )
        .await;
        assert_eq!(s, StatusCode::METHOD_NOT_ALLOWED, "{method}");
        assert_eq!(h[header::ALLOW], "GET, HEAD", "{method}");
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn an_etag_revalidates_and_head_carries_no_body() {
    let app = broker(None, &view(&READS), Some(&schema()))
        .await
        .expect("a broker");
    let (s, h, body) = get(&app, "schema/v2/model.shacl.ttl").await;
    assert_eq!(s, StatusCode::OK);
    let etag = h[header::ETAG].to_str().expect("an etag").to_owned();
    assert!(
        etag.starts_with('"') && etag.len() == 66,
        "strong sha-256: {etag}"
    );
    assert_eq!(h[header::CACHE_CONTROL], "private, no-cache");
    let uri = "/x/views/cars/linkml/schema/v2/model.shacl.ttl";
    let (s, _, empty) = send(&app, "GET", uri, &[("If-None-Match", &etag)], None).await;
    assert_eq!((s, empty.as_str()), (StatusCode::NOT_MODIFIED, ""));
    let (s, _, _) = send(&app, "GET", uri, &[("If-None-Match", "\"other\"")], None).await;
    assert_eq!(s, StatusCode::OK);
    let (s, h, empty) = send(&app, "HEAD", uri, &[], None).await;
    assert_eq!((s, empty.as_str()), (StatusCode::OK, ""));
    assert_eq!(h[header::CONTENT_LENGTH], body.len().to_string().as_str());
    assert_eq!(h[header::ETAG], etag.as_str());
}

#[tokio::test(flavor = "multi_thread")]
async fn each_path_is_an_operation_the_view_allows() {
    let app = broker(None, &view(&["retrieve_entity_type_info"]), Some(&schema()))
        .await
        .expect("a broker");
    assert_eq!(get(&app, "classes/Car").await.0, StatusCode::OK);
    for path in ["schema/index.json", "schema/v2/model", "slots/speed"] {
        let (s, _, body) = get(&app, path).await;
        assert_eq!(s, StatusCode::METHOD_NOT_ALLOWED, "{path}: {body}");
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn linkml_schema_is_cached_per_view_and_subject_and_never_served_across_them() {
    let engine = Arc::new(OmitSpeed(AtomicBool::new(false)));
    let app = broker(Some(engine.clone()), &view(&READS), Some(&schema()))
        .await
        .expect("a broker");
    let (_, wide_tag, wide) = get(&app, "schema/v2/model.linkml.yaml").await;
    assert!(wide.contains("\"speed\":"));
    engine.0.store(true, Ordering::SeqCst);
    let (_, narrow_tag, narrow) = get(&app, "schema/v2/model.linkml.yaml").await;
    assert!(
        !narrow.contains("\"speed\""),
        "the engine's narrowing holds: {narrow}"
    );
    assert_ne!(wide_tag[header::ETAG], narrow_tag[header::ETAG]);
    let (s, _, _) = send(
        &app,
        "GET",
        "/x/views/cars/linkml/schema/v2/model.linkml.yaml",
        &[(
            "If-None-Match",
            wide_tag[header::ETAG].to_str().expect("tag"),
        )],
        None,
    )
    .await;
    assert_eq!(
        s,
        StatusCode::OK,
        "a wider caller's tag does not revalidate"
    );
    engine.0.store(false, Ordering::SeqCst);
    assert_eq!(get(&app, "schema/v2/model.linkml.yaml").await.2, wide);
}

#[tokio::test(flavor = "multi_thread")]
async fn an_oversized_schema_is_refused_and_the_surface_stays_closed() {
    let mut big = schema();
    big["classes"]["Car"]["description"] = json!("x".repeat(SCHEMA_MAX));
    let err = broker(None, &view(&READS), Some(&big))
        .await
        .expect_err("past the bound");
    assert!(err.contains("413"), "{err}");
    let app = broker(None, &view(&READS), None).await.expect("a broker");
    let (s, _, body) = get(&app, "schema/index.json").await;
    assert_eq!(s, StatusCode::NOT_FOUND, "{body}");
    let (s, _, health) = send(&app, "GET", "/q/health", &[], None).await;
    assert_eq!(s, StatusCode::OK);
    assert!(health.contains("/x/views/{view}/linkml"), "{health}");
    let (s, _, _) = send(&app, "GET", "/x/views/cars/linkml", &[], None).await;
    assert_eq!(
        s,
        StatusCode::NOT_FOUND,
        "the surface root names no document"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_schema_outside_the_subset_or_none_closes_the_surface() {
    let mut outside = schema();
    outside["imports"] = json!(["linkml:types"]);
    let app = broker(None, &view(&READS), Some(&outside))
        .await
        .expect("a broker");
    assert_eq!(
        get(&app, "schema/index.json").await.0,
        StatusCode::NOT_FOUND
    );
    let app = broker(None, &view(&READS), None).await.expect("a broker");
    assert_eq!(
        get(&app, "schema/index.json").await.0,
        StatusCode::NOT_FOUND
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn linkml_a_relationship_to_a_hidden_class_is_uriorcurie() {
    let app = broker(None, &view(&READS), Some(&schema()))
        .await
        .expect("a broker");
    let (s, _, owner) = get(&app, "slots/owner").await;
    assert_eq!(s, StatusCode::OK, "{owner}");
    let owner: Value = serde_json::from_str(&owner).expect("json");
    assert_eq!(
        (owner["range"].as_str(), owner["kind"].as_str()),
        (Some("uriorcurie"), Some("Relationship"))
    );
    let (_, _, yaml) = get(&app, "schema/v2/model.linkml.yaml").await;
    assert!(yaml.contains("\"range\": \"uriorcurie\""), "{yaml}");
    assert_no_hidden_name("the YAML", &yaml);
}

/// Denies Retrieve Entity Type Information about `Car` and Retrieve
/// Attribute Information about `speed`, allows the rest.
struct NotCarNotSpeed;

impl PolicyEngine for NotCarNotSpeed {
    fn name(&self) -> &'static str {
        "not-car-not-speed"
    }
    fn decide<'a>(&'a self, _: &'a Subject, op: &'a Operation<'a>) -> DecisionFuture<'a> {
        let named = |names: &[String], iri: &str| names.iter().any(|n| n == iri);
        let d = if (op.clause == "5.7.7" && named(op.types, "https://e.org/city/Car"))
            || (op.clause == "5.7.10" && named(op.attrs, "https://e.org/city/speed"))
        {
            Decision::Deny("not this one".into())
        } else {
            Decision::Allow
        };
        Box::pin(std::future::ready(d))
    }
    fn pre_notify(&self, _: &Subject, _: &Value, _: &mut Value) -> NotifyDecision {
        NotifyDecision::Deliver
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn a_class_or_slot_request_names_it_to_the_engine() {
    let app = broker(
        Some(Arc::new(NotCarNotSpeed)),
        &view(&READS),
        Some(&schema()),
    )
    .await
    .expect("a broker");
    assert_eq!(get(&app, "classes/Car").await.0, StatusCode::FORBIDDEN);
    assert_eq!(get(&app, "slots/speed").await.0, StatusCode::FORBIDDEN);
    assert_eq!(get(&app, "slots/fuel").await.0, StatusCode::OK);
    assert_eq!(get(&app, "schema/index.json").await.0, StatusCode::OK);
    // A name the schema lacks never reaches the engine: the same 404.
    assert_eq!(get(&app, "classes/Nothing").await.0, StatusCode::NOT_FOUND);
}

#[tokio::test(flavor = "multi_thread")]
async fn model_answers_the_formalism_accept_names() {
    let app = broker(None, &view(&READS), Some(&schema()))
        .await
        .expect("a broker");
    let uri = "/x/views/cars/linkml/schema/v2/model";
    for (accept, media_type) in [
        (
            "text/turtle; profile=\"owl\"",
            "text/turtle; profile=\"owl\"",
        ),
        ("text/yaml", "text/yaml"),
        ("application/json", "application/schema+json"),
    ] {
        let (s, h, _) = send(&app, "GET", uri, &[("Accept", accept)], None).await;
        assert_eq!(s, StatusCode::OK, "{accept}");
        assert_eq!(h[header::CONTENT_TYPE], media_type, "{accept}");
    }
}
