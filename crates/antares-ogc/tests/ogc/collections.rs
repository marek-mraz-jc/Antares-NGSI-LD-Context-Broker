// SPDX-License-Identifier: EUPL-1.2
//! Part 1 Core: `/collections` and `/collections/{c}` (mapping rows C5 to C7, M1, M8).

use crate::common::{fixture, get, href, send};
use axum::http::StatusCode;
use serde_json::Value;

#[tokio::test(flavor = "multi_thread")]
async fn lists_the_view_types_only() -> Result<(), String> {
    let f = fixture().await?;
    let a = get(&f.app, "/x/views/city/ogc/features/collections").await;
    assert_eq!(a.status, StatusCode::OK, "{}", a.body);
    let ids: Vec<&str> = a
        .body
        .get("collections")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|c| c.get("id").and_then(Value::as_str))
        .collect();
    assert_eq!(
        ids,
        ["Car", "Stop"],
        "Depot is in the schema, not in the view"
    );
    assert!(!a.body.to_string().contains("Depot"));
    assert!(!a.body.to_string().contains("secret"));
    let a = get(&f.app, "/x/views/empty/ogc/features/collections").await;
    let ids: Vec<&str> = a
        .body
        .get("collections")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|c| c.get("id").and_then(Value::as_str))
        .collect();
    assert_eq!(ids, ["Stop"]);
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn no_extent_member() -> Result<(), String> {
    let f = fixture().await?;
    let a = get(&f.app, "/x/views/city/ogc/features/collections/Car").await;
    assert_eq!(a.status, StatusCode::OK, "{}", a.body);
    assert_eq!(a.body.get("id").and_then(Value::as_str), Some("Car"));
    assert_eq!(
        a.body.get("description").and_then(Value::as_str),
        Some("A car of the fleet")
    );
    assert_eq!(
        a.body.get("itemType").and_then(Value::as_str),
        Some("feature")
    );
    assert!(a.body.get("extent").is_none(), "M8: no extent");
    assert_eq!(
        href(&a.body, "items").as_deref(),
        Some("http://localhost:9090/x/views/city/ogc/features/collections/Car/items")
    );
    assert!(get(&f.app, "/x/views/city/ogc/features/collections/Stop")
        .await
        .body
        .get("description")
        .is_none());
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn hidden_and_unknown_collection_are_the_same_404() -> Result<(), String> {
    let f = fixture().await?;
    let hidden = get(&f.app, "/x/views/city/ogc/features/collections/Depot").await;
    let unknown = get(&f.app, "/x/views/city/ogc/features/collections/Nope").await;
    assert_eq!(hidden.status, StatusCode::NOT_FOUND);
    assert_eq!(hidden.body, unknown.body);
    for tail in ["/items", "/items/urn:ngsi-ld:Depot:1"] {
        let hidden = get(
            &f.app,
            &format!("/x/views/city/ogc/features/collections/Depot{tail}"),
        )
        .await;
        let unknown = get(
            &f.app,
            &format!("/x/views/city/ogc/features/collections/Nope{tail}"),
        )
        .await;
        assert_eq!(
            (hidden.status, &hidden.body),
            (StatusCode::NOT_FOUND, &unknown.body),
            "{tail}"
        );
    }
    let car_in_empty = get(&f.app, "/x/views/empty/ogc/features/collections/Car").await;
    assert_eq!(
        car_in_empty.status,
        StatusCode::NOT_FOUND,
        "a type another view shows"
    );
    Ok(())
}

/// ADR-0023 rule 12: a view of `*` shows every class of the schema with every slot, as its
/// NGSI-LD root serves every Entity; its own `q` still narrows the items.
#[tokio::test(flavor = "multi_thread")]
async fn a_view_of_everything_serves_every_collection() -> Result<(), String> {
    let f = fixture().await?;
    let view = r#"{"id": "all", "tenant": "city", "selector": ["*"], "q": "speed<50",
                   "rules": [{"types": ["*"], "attributes": ["*"]}], "operations": ["query_entities"]}"#;
    let a = send(&f.app, "PUT", "/q/views/all", view, &[]).await;
    assert_eq!(a.status, StatusCode::CREATED, "{}", a.body);
    let a = get(&f.app, "/x/views/all/ogc/features/collections").await;
    let ids: Vec<&str> = a
        .body
        .get("collections")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|c| c.get("id").and_then(Value::as_str))
        .collect();
    assert_eq!(ids, ["Car", "Depot", "Stop"], "{}", a.body);
    let ids = |v: &Value, key: &str| -> Vec<String> {
        let mut out: Vec<String> = v
            .as_array()
            .or_else(|| v.get(key).and_then(Value::as_array))
            .into_iter()
            .flatten()
            .filter_map(|e| e.get("id").and_then(Value::as_str).map(str::to_owned))
            .collect();
        out.sort();
        out
    };
    let root = send(
        &f.app,
        "GET",
        "/ngsi-ld/v1/entities?type=Car&q=speed%3C50&limit=1000",
        "",
        &[("NGSILD-Tenant", "city")],
    )
    .await;
    let a = get(
        &f.app,
        "/x/views/all/ogc/features/collections/Car/items?limit=1000",
    )
    .await;
    assert_eq!(a.status, StatusCode::OK, "{}", a.body);
    let want = ids(&root.body, "");
    assert_eq!(
        (ids(&a.body, "features"), want.len()),
        (want.clone(), 2),
        "{}",
        a.body
    );
    assert_eq!(a.body.get("numberMatched").and_then(Value::as_u64), Some(2));
    assert!(
        a.body.pointer("/features/0/properties/secret").is_some(),
        "every slot shows"
    );
    Ok(())
}

/// T-3752 (dev, Helsinki): a schema whose classes and slots are another vocabulary's IRIs
/// (`sdm:`, `location` as `geojson:geometry`) while the view and the stored Entities use short
/// names read under the core @context. The collection is listed by its view name and its items
/// are the Entities written with short names, its geometry the NGSI-LD `location`.
#[tokio::test(flavor = "multi_thread")]
async fn a_schema_of_another_vocabulary_serves_the_entities_the_view_names() -> Result<(), String> {
    crate::common::schemas_on();
    let broker = antares_ops::test_kit::TestBroker::new().await?;
    let app = broker.serve(antares_ogc::Ogc)?;
    let schema = r#"{
      "id": "https://hel.fi/models/helsinki", "name": "helsinki", "default_prefix": "sdm",
      "prefixes": {"sdm": {"prefix_prefix": "sdm", "prefix_reference": "https://smartdatamodels.org/"},
                   "geojson": {"prefix_prefix": "geojson", "prefix_reference": "https://purl.org/geojson/vocab#"}},
      "classes": {"Station": {"class_uri": "sdm:Station", "slots": ["bikes", "location"]}},
      "slots": {"bikes": {"slot_uri": "sdm:bikes", "range": "integer"},
                "location": {"slot_uri": "geojson:geometry", "range": "string",
                             "annotations": {"ngsi_ld_kind": {"tag": "ngsi_ld_kind", "value": "GeoProperty"}}}}
    }"#;
    let view = serde_json::json!({"id": "bikes", "tenant": "hel", "selector": ["Station"],
        "rules": [{"types": ["Station"], "attributes": ["bikes", "location"]}],
        "operations": ["query_entities"]});
    let station = serde_json::json!({"id": "urn:ngsi-ld:Station:1", "type": "Station",
        "bikes": {"type": "Property", "value": 4},
        "location": {"type": "GeoProperty", "value": {"type": "Point", "coordinates": [24.95, 60.15]}}});
    for (method, uri, body, headers) in [
        ("PUT", "/q/views/bikes", view.to_string(), &[][..]),
        ("PUT", "/q/schemas/hel", schema.to_owned(), &[][..]),
        (
            "POST",
            "/ngsi-ld/v1/entities",
            station.to_string(),
            &[("NGSILD-Tenant", "hel")][..],
        ),
    ] {
        let a = send(&app, method, uri, &body, headers).await;
        assert_eq!(a.status, StatusCode::CREATED, "{uri}: {}", a.body);
    }
    let a = get(&app, "/x/views/bikes/ogc/features/collections").await;
    assert_eq!(a.status, StatusCode::OK, "{}", a.body);
    assert_eq!(
        a.body["collections"][0]["id"], "Station",
        "listed by the name the view shows: {}",
        a.body
    );
    let a = get(
        &app,
        "/x/views/bikes/ogc/features/collections/Station/items",
    )
    .await;
    assert_eq!(a.status, StatusCode::OK, "{}", a.body);
    assert_eq!(
        crate::common::ids(&a.body),
        ["urn:ngsi-ld:Station:1"],
        "{}",
        a.body
    );
    let feature = &a.body["features"][0];
    assert_eq!(feature["geometry"]["type"], "Point", "{feature}");
    assert_eq!(feature["properties"]["bikes"], 4, "{feature}");
    drop(broker);
    Ok(())
}
