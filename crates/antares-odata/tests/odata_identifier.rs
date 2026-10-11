// SPDX-License-Identifier: EUPL-1.2
//! A key from the model (odata-linkml-mapping row E2a, T-3720): a class's
//! LinkML `identifier` slot is the EntityType's key, its value `v` names
//! the Entity `urn:ngsi-ld:{Class}:{v}`, on every keyed path and in every
//! id the surface writes. A value never names an Entity of another set,
//! and `(id='urn:…')` still names one whose URN does not follow the pattern.

use antares_ops::test_kit::TestBroker;
use axum::body::Body;
use axum::http::{header::LOCATION, Method, Request, StatusCode};
use axum::Router;
use http_body_util::BodyExt;
use serde_json::{json, Value};
use tower::ServiceExt;

const SHOP: &str = "https://example.org/shop/";
const ROOT: &str = "/x/views/shop/odata/v4";
const GUID: &str = "2dcd1e3c-e38a-5391-ba63-17ecfeaea3c2";
const OTHER: &str = "9b0b6a3e-0c54-4f5e-9a0d-6a3c2c7f1e11";
const FULL: &str = "$format=application/json;odata.metadata=full";

const SCHEMA: &str = r#"{
  "id": "https://example.org/shop", "name": "shop",
  "prefixes": {"shop": {"prefix_prefix": "shop", "prefix_reference": "https://example.org/shop/"}},
  "default_prefix": "shop",
  "classes": {"Product": {"slots": ["ID", "Name", "category"]},
              "Category": {"slots": ["code", "Name"]},
              "Bin": {"slots": ["number"]}},
  "slots": {"ID": {"identifier": true, "annotations": {"edm_type": {"tag": "edm_type", "value": "Edm.Guid"}}},
            "Name": {}, "category": {"range": "Category"},
            "code": {"identifier": true},
            "number": {"identifier": true, "range": "integer"}}
}"#;

struct Answer {
    status: StatusCode,
    location: String,
    body: Value,
}

async fn send(app: &Router, method: Method, uri: &str, body: Option<&str>) -> Answer {
    let mut req = Request::builder()
        .method(method)
        .uri(uri)
        .header("NGSILD-Tenant", "shop");
    if uri.ends_with("$metadata?$format=json") {
        // The JSON metadata document is a 4.01 one.
        req = req.header("OData-MaxVersion", "4.01");
    }
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
    let location = resp
        .headers()
        .get(LOCATION)
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default()
        .to_owned();
    let bytes = resp
        .into_body()
        .collect()
        .await
        .map(http_body_util::Collected::to_bytes)
        .unwrap_or_default();
    let text = String::from_utf8_lossy(&bytes).into_owned();
    Answer {
        status,
        location,
        body: serde_json::from_str(&text).unwrap_or(Value::String(text)),
    }
}

async fn odata(app: &Router, method: Method, path: &str, body: Option<&str>) -> Answer {
    send(app, method, &format!("{ROOT}/{path}"), body).await
}

fn property(value: &Value) -> Value {
    json!({"type": "Property", "value": value})
}

/// Tenant `shop`: one Product keyed by `GUID` in Category `c1`, one Product
/// whose URN does not follow the pattern, one Bin keyed by an integer.
async fn shop() -> Result<(TestBroker, Router), String> {
    std::env::set_var(antares_ops::views::SCHEMA_MAX_ENV, "65536");
    let b = TestBroker::new().await?;
    let app = b.serve(antares_odata::OData)?;
    let view = json!({
        "id": "shop", "tenant": "shop", "@context": {"shop": SHOP},
        "selector": ["shop:Product", "shop:Category", "shop:Bin"],
        "rules": [{"types": ["shop:Product"], "attributes": ["shop:ID", "shop:Name", "shop:category"]},
                  {"types": ["shop:Category"], "attributes": ["shop:code", "shop:Name"]},
                  {"types": ["shop:Bin"], "attributes": ["shop:number"]}],
        "operations": ["query_entities", "retrieve_entity", "retrieve_attribute_value",
                       "create_entity", "merge_entity", "delete_entity"]
    });
    for (uri, body) in [
        ("/q/views/shop", view.to_string()),
        ("/q/schemas/shop", SCHEMA.to_owned()),
    ] {
        let a = send(&app, Method::PUT, uri, Some(&body)).await;
        if a.status != StatusCode::CREATED {
            return Err(format!("{uri}: {} {}", a.status, a.body));
        }
    }
    let entities = [
        json!({"id": "urn:ngsi-ld:Category:c1", "type": format!("{SHOP}Category"),
               format!("{SHOP}code"): property(&json!("c1")),
               format!("{SHOP}Name"): property(&json!("Tools"))}),
        json!({"id": format!("urn:ngsi-ld:Product:{GUID}"), "type": format!("{SHOP}Product"),
               format!("{SHOP}ID"): property(&json!(GUID)),
               format!("{SHOP}Name"): property(&json!("Drill")),
               format!("{SHOP}category"): {"type": "Relationship", "object": "urn:ngsi-ld:Category:c1"}}),
        json!({"id": "urn:ngsi-ld:Product:odd", "type": format!("{SHOP}Product"),
               format!("{SHOP}Name"): property(&json!("Odd"))}),
        json!({"id": "urn:ngsi-ld:Bin:7", "type": format!("{SHOP}Bin"),
               format!("{SHOP}number"): property(&json!(7))}),
    ];
    for entity in entities {
        let a = send(
            &app,
            Method::POST,
            "/ngsi-ld/v1/entities",
            Some(&entity.to_string()),
        )
        .await;
        if a.status != StatusCode::CREATED {
            return Err(format!("{:?}: {} {}", entity.get("id"), a.status, a.body));
        }
    }
    Ok((b, app))
}

#[tokio::test(flavor = "multi_thread")]
async fn metadata_names_the_identifier_as_the_key() {
    let (_b, app) = shop().await.expect("broker");
    let a = odata(&app, Method::GET, "$metadata?$format=json", None).await;
    assert_eq!(a.status, StatusCode::OK, "{}", a.body);
    let ns = &a.body["shop"];
    let product = &ns["Product"];
    assert_eq!(product["$Key"], json!(["ID"]), "{product}");
    assert_eq!(product["ID"]["$Type"], "Edm.Guid", "{product}");
    assert_eq!(
        product["ID"]["$Nullable"],
        Value::Null,
        "a key is never null"
    );
    assert_eq!(product["$BaseType"], Value::Null, "{product}");
    assert_eq!(product["id"]["$Type"], Value::Null, "id is a String member");
    assert!(product.get("_types").is_some(), "{product}");
    assert_eq!(ns["Category"]["$Key"], json!(["code"]));
    assert_eq!(ns["Bin"]["number"]["$Type"], "Edm.Int64");
}

#[tokio::test(flavor = "multi_thread")]
async fn every_key_form_reads_the_entity_its_value_names() {
    let (_b, app) = shop().await.expect("broker");
    let upper = GUID.to_ascii_uppercase();
    for path in [
        format!("Product({GUID})"),
        format!("Product(ID={GUID})"),
        format!("Product({upper})"),
        format!("Product(@k)?@k={GUID}"),
        format!("Product(id='urn:ngsi-ld:Product:{GUID}')"),
        format!("Product('urn:ngsi-ld:Product:{GUID}')"),
    ] {
        let a = odata(&app, Method::GET, &path, None).await;
        assert_eq!(a.status, StatusCode::OK, "{path}: {}", a.body);
        assert_eq!(a.body["ID"], GUID, "{path}: {}", a.body);
        assert_eq!(
            a.body["id"],
            format!("urn:ngsi-ld:Product:{GUID}"),
            "{path}"
        );
    }
    let a = odata(&app, Method::GET, &format!("Product({GUID})?{FULL}"), None).await;
    assert_eq!(
        a.body["@odata.id"],
        format!("Product({GUID})"),
        "{}",
        a.body
    );
    let a = odata(&app, Method::GET, "Category('c1')", None).await;
    assert_eq!(a.body["Name"], "Tools", "{}", a.body);
    let a = odata(&app, Method::GET, &format!("Bin(7)?{FULL}"), None).await;
    assert_eq!(a.body["@odata.id"], "Bin(7)", "{}", a.body);
}

#[tokio::test(flavor = "multi_thread")]
async fn paths_below_the_key_and_references_use_it() {
    let (_b, app) = shop().await.expect("broker");
    let a = odata(&app, Method::GET, &format!("Product({GUID})/Name"), None).await;
    assert_eq!(a.status, StatusCode::OK, "{}", a.body);
    assert_eq!(a.body["value"], "Drill");
    assert!(
        a.body["@odata.context"]
            .as_str()
            .is_some_and(|c| c.ends_with(&format!("#Product({GUID})/Name"))),
        "{}",
        a.body
    );
    let a = odata(
        &app,
        Method::GET,
        &format!("Product({GUID})/ID/$value"),
        None,
    )
    .await;
    assert_eq!(a.body, Value::String(GUID.into()));
    let a = odata(
        &app,
        Method::GET,
        &format!("Product({GUID})/category/$ref"),
        None,
    )
    .await;
    assert_eq!(a.status, StatusCode::OK, "{}", a.body);
    assert!(
        a.body["@odata.id"]
            .as_str()
            .is_some_and(|id| id.ends_with("/Category('c1')")),
        "{}",
        a.body
    );
    let a = odata(
        &app,
        Method::GET,
        &format!("Product?$expand=category/$ref&$filter=ID%20eq%20{GUID}"),
        None,
    )
    .await;
    assert_eq!(
        a.body["value"][0]["category"]["@odata.id"], "Category('c1')",
        "{}",
        a.body
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_key_never_names_an_entity_of_another_set() {
    let (_b, app) = shop().await.expect("broker");
    for path in [
        format!("Category('{GUID}')"),
        format!("Category(code='{GUID}')"),
        "Product('urn:ngsi-ld:Category:c1')".to_owned(),
        "Product(id='urn:ngsi-ld:Category:c1')".to_owned(),
        format!("Product({OTHER})"),
        "Bin(8)".to_owned(),
        // A literal of another type than the key's names nothing.
        format!("Bin({GUID})"),
        "Product(7)".to_owned(),
    ] {
        let a = odata(&app, Method::GET, &path, None).await;
        assert_eq!(a.status, StatusCode::NOT_FOUND, "{path}: {}", a.body);
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn an_urn_outside_the_pattern_is_addressed_by_its_id() {
    let (_b, app) = shop().await.expect("broker");
    let a = odata(
        &app,
        Method::GET,
        &format!("Product?$orderby=Name&{FULL}"),
        None,
    )
    .await;
    assert_eq!(a.status, StatusCode::OK, "{}", a.body);
    let ids: Vec<&str> = a.body["value"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|e| e["@odata.id"].as_str())
        .collect();
    assert_eq!(
        ids,
        [
            format!("Product({GUID})").as_str(),
            "Product(id='urn:ngsi-ld:Product:odd')"
        ],
        "{}",
        a.body
    );
    let a = odata(
        &app,
        Method::GET,
        "Product(id='urn:ngsi-ld:Product:odd')",
        None,
    )
    .await;
    assert_eq!(a.body["Name"], "Odd", "{}", a.body);
}

#[tokio::test(flavor = "multi_thread")]
async fn writes_address_and_check_the_key() {
    let (_b, app) = shop().await.expect("broker");
    let body = json!({"ID": OTHER, "Name": "Saw"}).to_string();
    let a = odata(&app, Method::POST, "Product", Some(&body)).await;
    assert_eq!(a.status, StatusCode::CREATED, "{}", a.body);
    assert!(
        a.location.ends_with(&format!("/Product({OTHER})")),
        "{}",
        a.location
    );
    let stored = send(
        &app,
        Method::GET,
        &format!("/ngsi-ld/v1/entities/urn:ngsi-ld:Product:{OTHER}"),
        None,
    )
    .await;
    assert_eq!(stored.status, StatusCode::OK, "{}", stored.body);
    assert_eq!(
        stored.body[format!("{SHOP}ID")]["value"],
        OTHER,
        "{}",
        stored.body
    );

    let wrong = json!({"ID": GUID, "Name": "x"}).to_string();
    let a = odata(
        &app,
        Method::PATCH,
        &format!("Product({OTHER})"),
        Some(&wrong),
    )
    .await;
    assert_eq!(a.status, StatusCode::BAD_REQUEST, "{}", a.body);
    let same = json!({"ID": OTHER.to_ascii_uppercase(), "Name": "Big saw"}).to_string();
    let a = odata(
        &app,
        Method::PATCH,
        &format!("Product({OTHER})"),
        Some(&same),
    )
    .await;
    assert_eq!(a.status, StatusCode::NO_CONTENT, "{}", a.body);
    let a = odata(&app, Method::GET, &format!("Product({OTHER})/Name"), None).await;
    assert_eq!(a.body["value"], "Big saw", "{}", a.body);

    let a = odata(&app, Method::DELETE, &format!("Product({OTHER})"), None).await;
    assert_eq!(a.status, StatusCode::NO_CONTENT, "{}", a.body);
    let a = odata(&app, Method::GET, &format!("Product({OTHER})"), None).await;
    assert_eq!(a.status, StatusCode::NOT_FOUND, "{}", a.body);
}

#[tokio::test(flavor = "multi_thread")]
async fn entity_dereferences_an_id_of_this_service() {
    // Q23: `$entity?$id=` reads the Entity the id names, as its URL does.
    let (_b, app) = shop().await.expect("broker");
    let a = odata(&app, Method::GET, &format!("Product({GUID})?{FULL}"), None).await;
    let root = a.body["@odata.context"]
        .as_str()
        .and_then(|c| c.split_once("$metadata"))
        .map(|(root, _)| root.to_owned())
        .unwrap_or_default();
    assert!(root.ends_with("/odata/v4/"), "{}", a.body);
    let relative = format!("Product({GUID})");
    let absolute = format!("{root}{relative}");
    for id in [
        relative.clone(),
        absolute.clone(),
        relative.replace('(', "%28").replace(')', "%29"),
        absolute
            .replace(':', "%3A")
            .replace('/', "%2F")
            .replace('(', "%28"),
        "Category('c1')".to_owned(),
        format!("Product(id='urn:ngsi-ld:Product:{GUID}')"),
    ] {
        let a = odata(&app, Method::GET, &format!("$entity?$id={id}"), None).await;
        assert_eq!(a.status, StatusCode::OK, "{id}: {}", a.body);
        assert!(a.body["Name"].is_string(), "{id}: {}", a.body);
    }
    let a = odata(&app, Method::GET, &format!("$entity?$id={OTHER}"), None).await;
    assert_eq!(a.status, StatusCode::BAD_REQUEST, "a bare key: {}", a.body);
    let a = odata(
        &app,
        Method::GET,
        &format!("$entity?$id=Product({OTHER})"),
        None,
    )
    .await;
    assert_eq!(a.status, StatusCode::NOT_FOUND, "{}", a.body);
    for id in [
        format!("http://elsewhere.example/{relative}"),
        format!("/{relative}"),
        "Product".to_owned(),
        format!("{relative}/Name"),
        format!("{relative}%3F%24select%3DName"),
        "Product%2".to_owned(),
        "Product%zz".to_owned(),
        "%FF".to_owned(),
        "Nothing('x')".to_owned(),
    ] {
        let a = odata(&app, Method::GET, &format!("$entity?$id={id}"), None).await;
        assert_eq!(a.status, StatusCode::BAD_REQUEST, "{id}: {}", a.body);
    }
}

/// W1 (T-3721): a create that leaves out a `Guid` or `String` key gets a
/// uuid as its key value and the URN from it, the identifier stored as its
/// own Attribute; an `Int64` key has no such default and stays required.
#[tokio::test(flavor = "multi_thread")]
async fn a_create_without_the_key_mints_its_value() {
    let (_b, app) = shop().await.expect("broker");
    let uuid_of = |location: &str, set: &str, quote: &str| {
        let value = location
            .rsplit_once(&format!("/{set}({quote}"))
            .and_then(|(_, rest)| rest.strip_suffix(&format!("{quote})")))
            .unwrap_or_default()
            .to_owned();
        assert_eq!(
            (value.len(), value.split('-').count()),
            (36, 5),
            "{location}"
        );
        value
    };
    let a = odata(&app, Method::POST, "Product", Some(r#"{"Name": "Saw"}"#)).await;
    assert_eq!(a.status, StatusCode::CREATED, "{}", a.body);
    let guid = uuid_of(&a.location, "Product", "");
    assert_eq!(a.body["ID"], guid.as_str(), "{}", a.body);
    let stored = send(
        &app,
        Method::GET,
        &format!("/ngsi-ld/v1/entities/urn:ngsi-ld:Product:{guid}"),
        None,
    )
    .await;
    assert_eq!(
        stored.body[format!("{SHOP}ID")]["value"],
        guid.as_str(),
        "{}",
        stored.body
    );

    let a = odata(&app, Method::POST, "Category", Some(r#"{"Name": "Saws"}"#)).await;
    assert_eq!(a.status, StatusCode::CREATED, "{}", a.body);
    let code = uuid_of(&a.location, "Category", "'");
    let read = odata(&app, Method::GET, &format!("Category('{code}')"), None).await;
    assert_eq!(read.body["Name"], "Saws", "{}", read.body);

    let a = odata(&app, Method::POST, "Bin", Some("{}")).await;
    assert_eq!(
        (a.status, a.body["error"]["code"].as_str()),
        (StatusCode::BAD_REQUEST, Some("MissingKey")),
        "{}",
        a.body
    );

    let a = odata(&app, Method::GET, "$metadata?$format=json", None).await;
    let ns = &a.body["shop"];
    for (ty, key, minted) in [
        ("Product", "ID", true),
        ("Category", "code", true),
        ("Bin", "number", false),
    ] {
        assert_eq!(
            ns[ty][key]["@Core.ComputedDefaultValue"].as_bool(),
            minted.then_some(true),
            "{ty}.{key}: {}",
            ns[ty]
        );
    }
}
