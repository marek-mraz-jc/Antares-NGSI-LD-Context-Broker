// SPDX-License-Identifier: EUPL-1.2
//! Part 1 `GeoJSON` (mapping rows C20, M2 to M5).

use crate::common::{fixture, get, href};
use axum::http::StatusCode;
use serde_json::json;

#[tokio::test(flavor = "multi_thread")]
async fn feature_collection_and_feature_shape() -> Result<(), String> {
    let f = fixture().await?;
    let one = get(
        &f.app,
        "/x/views/city/ogc/features/collections/Car/items/urn:ngsi-ld:Car:1",
    )
    .await;
    assert_eq!(one.status, StatusCode::OK, "{}", one.body);
    assert_eq!(
        one.headers
            .get("content-type")
            .map(axum::http::HeaderValue::as_bytes),
        Some(&b"application/geo+json"[..])
    );
    assert_eq!(one.body.get("type"), Some(&json!("Feature")));
    assert_eq!(
        one.body.get("geometry"),
        Some(&json!({"type": "Point", "coordinates": [17.1, 48.1]}))
    );
    assert_eq!(
        one.body.get("properties"),
        Some(&json!({"speed": 10, "seen": "2026-01-01T00:00:00Z"})),
        "the geometry is not a property; secret is hidden"
    );
    for rel in ["self", "collection"] {
        assert!(href(&one.body, rel).is_some(), "{rel}");
    }
    let no_place = get(
        &f.app,
        "/x/views/city/ogc/features/collections/Car/items/urn:ngsi-ld:Car:3",
    )
    .await;
    assert_eq!(
        no_place.body.get("geometry"),
        Some(&json!(null)),
        "M4: no location is geometry null"
    );
    assert_eq!(no_place.body.get("properties"), Some(&json!({"speed": 30})));
    let stop = get(
        &f.app,
        "/x/views/city/ogc/features/collections/Stop/items/urn:ngsi-ld:Stop:1",
    )
    .await;
    assert_eq!(
        (stop.body.get("geometry"), stop.body.get("properties")),
        (Some(&json!(null)), Some(&json!({"name": "Hlavná"})))
    );
    let page = get(&f.app, "/x/views/city/ogc/features/collections/Car/items").await;
    let members: Vec<&String> = page
        .body
        .as_object()
        .map(|m| m.keys().collect())
        .unwrap_or_default();
    assert_eq!(
        members,
        [
            "features",
            "links",
            "numberMatched",
            "numberReturned",
            "type"
        ],
        "C17: no timeStamp"
    );
    assert_eq!(page.body.get("type"), Some(&json!("FeatureCollection")));
    Ok(())
}
