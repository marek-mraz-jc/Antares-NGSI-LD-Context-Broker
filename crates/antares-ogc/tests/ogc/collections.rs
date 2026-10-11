// SPDX-License-Identifier: EUPL-1.2
//! Part 1 Core: `/collections` and `/collections/{c}` (mapping rows C5 to C7, M1, M8).

use crate::common::{fixture, get, href};
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
