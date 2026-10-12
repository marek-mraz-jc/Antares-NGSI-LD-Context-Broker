// SPDX-License-Identifier: EUPL-1.2
//! Part 1 Core: `/items` and `/items/{featureId}` (mapping rows C8, C10 to C14, C16, C18,
//! C19), through the view and through a narrowing engine.

use crate::common::{fixture, get, href, ids, local, narrowed, send, SCHEMA};
use axum::http::StatusCode;
use serde_json::{json, Value};

const CARS: &str = "/x/views/city/ogc/features/collections/Car/items";

#[tokio::test(flavor = "multi_thread")]
async fn items_are_the_view_answer() -> Result<(), String> {
    let f = fixture().await?;
    let a = get(&f.app, CARS).await;
    assert_eq!(a.status, StatusCode::OK, "{}", a.body);
    let mut got = ids(&a.body);
    got.sort();
    assert_eq!(
        got,
        [
            "urn:ngsi-ld:Car:1",
            "urn:ngsi-ld:Car:2",
            "urn:ngsi-ld:Car:3",
            "urn:ngsi-ld:Car:4"
        ]
    );
    assert!(
        !a.body.to_string().contains("secret") && !a.body.to_string().contains("\"s1\""),
        "a hidden Attribute appears nowhere"
    );
    let slow = get(&f.app, "/x/views/slow/ogc/features/collections/Car/items").await;
    let mut got = ids(&slow.body);
    got.sort();
    assert_eq!(
        got,
        ["urn:ngsi-ld:Car:1", "urn:ngsi-ld:Car:3"],
        "the view's q narrows"
    );
    assert_eq!(slow.body.get("numberMatched"), Some(&json!(2)));
    let stops = get(&f.app, "/x/views/empty/ogc/features/collections/Stop/items").await;
    assert_eq!(ids(&stops.body), ["urn:ngsi-ld:Stop:1"]);
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn an_engine_narrows_items_counts_links_and_items_by_id() -> Result<(), String> {
    let f = narrowed().await?;
    let a = get(&f.app, &format!("{CARS}?limit=1")).await;
    assert_eq!(a.status, StatusCode::OK, "{}", a.body);
    assert_eq!(
        a.body.get("numberMatched"),
        Some(&json!(2)),
        "Cars 2 and 4 are not counted"
    );
    let mut seen = ids(&a.body);
    let next = href(&a.body, "next").ok_or("a next link")?;
    let b = get(&f.app, &local(&next)).await;
    seen.extend(ids(&b.body));
    seen.sort();
    assert_eq!(seen, ["urn:ngsi-ld:Car:1", "urn:ngsi-ld:Car:3"]);
    assert!(
        href(&b.body, "next").is_none(),
        "no link past the narrowed set"
    );
    let hidden = get(&f.app, &format!("{CARS}/urn:ngsi-ld:Car:2")).await;
    let absent = get(&f.app, &format!("{CARS}/urn:ngsi-ld:Car:99")).await;
    assert_eq!(
        (hidden.status, &hidden.body),
        (StatusCode::NOT_FOUND, &absent.body),
        "hidden reads as absent"
    );
    let boxed = get(&f.app, &format!("{CARS}?bbox=179,-1,-179,1")).await;
    assert_eq!(
        ids(&boxed.body),
        ["urn:ngsi-ld:Car:3"],
        "only the Car without location: 2 and 4 are hidden"
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn limit_default_clamp_and_bounds() -> Result<(), String> {
    let f = fixture().await?;
    let one = get(&f.app, &format!("{CARS}?limit=1")).await;
    assert_eq!(
        (
            one.body.get("numberReturned"),
            one.body.get("numberMatched")
        ),
        (Some(&json!(1)), Some(&json!(4)))
    );
    let all = get(&f.app, &format!("{CARS}?limit=10000")).await;
    assert_eq!(
        all.status,
        StatusCode::OK,
        "10000 is clamped, not refused: {}",
        all.body
    );
    assert_eq!(all.body.get("numberReturned"), Some(&json!(4)));
    assert!(href(&all.body, "next").is_none());
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn bbox_2d_3d_and_antimeridian() -> Result<(), String> {
    let f = fixture().await?;
    let ask = |q: &str| format!("{CARS}?bbox={q}");
    let around = |body: &Value| {
        let mut v = ids(body);
        v.sort();
        v
    };
    let a = get(&f.app, &ask("17,48,18,49")).await;
    assert_eq!(a.status, StatusCode::OK, "{}", a.body);
    assert_eq!(
        around(&a.body),
        ["urn:ngsi-ld:Car:1", "urn:ngsi-ld:Car:3"],
        "Car 1 inside, Car 3 has no geometry"
    );
    assert_eq!(
        around(&get(&f.app, &ask("17,48,-100,18,49,100")).await.body),
        ["urn:ngsi-ld:Car:1", "urn:ngsi-ld:Car:3"],
        "6 numbers"
    );
    assert_eq!(
        around(&get(&f.app, &ask("179,-1,-179,1")).await.body),
        [
            "urn:ngsi-ld:Car:2",
            "urn:ngsi-ld:Car:3",
            "urn:ngsi-ld:Car:4"
        ],
        "across 180"
    );
    assert_eq!(
        around(&get(&f.app, &ask("0,-1,1,1")).await.body),
        ["urn:ngsi-ld:Car:3"]
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn bbox_keeps_features_without_geometry() -> Result<(), String> {
    let f = fixture().await?;
    let a = get(&f.app, &format!("{CARS}?bbox=0,-1,1,1&limit=1")).await;
    assert_eq!(ids(&a.body), ["urn:ngsi-ld:Car:3"]);
    assert_eq!(a.body.get("numberMatched"), Some(&json!(1)));
    let stops = get(
        &f.app,
        "/x/views/city/ogc/features/collections/Stop/items?bbox=0,0,1,1",
    )
    .await;
    assert_eq!(
        ids(&stops.body),
        ["urn:ngsi-ld:Stop:1"],
        "a collection without geometry matches whole"
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn datetime_instant_interval_open_ends() -> Result<(), String> {
    let f = fixture().await?;
    for (datetime, want) in [
        ("2026-01-01T00:00:00Z", vec!["urn:ngsi-ld:Car:1"]),
        (
            "2025-12-01T00:00:00Z/2026-02-01T00:00:00Z",
            vec!["urn:ngsi-ld:Car:1"],
        ),
        ("2026-03-01T00:00:00Z/..", vec!["urn:ngsi-ld:Car:2"]),
        ("/2026-03-01T00:00:00Z", vec!["urn:ngsi-ld:Car:1"]),
        ("2027-01-01T00:00:00Z/", vec![]),
    ] {
        let a = get(&f.app, &format!("{CARS}?datetime={datetime}")).await;
        assert_eq!(a.status, StatusCode::OK, "{datetime}: {}", a.body);
        let mut got: Vec<String> = ids(&a.body)
            .into_iter()
            .filter(|id| id != "urn:ngsi-ld:Car:3" && id != "urn:ngsi-ld:Car:4")
            .collect();
        got.sort();
        assert_eq!(got, want, "{datetime}");
    }
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn datetime_keeps_features_without_time() -> Result<(), String> {
    let f = fixture().await?;
    let a = get(&f.app, &format!("{CARS}?datetime=2027-01-01T00:00:00Z")).await;
    let mut got = ids(&a.body);
    got.sort();
    assert_eq!(
        got,
        ["urn:ngsi-ld:Car:3", "urn:ngsi-ld:Car:4"],
        "never seen: no temporal geometry"
    );
    let stops = get(
        &f.app,
        "/x/views/city/ogc/features/collections/Stop/items?datetime=2027-01-01T00:00:00Z",
    )
    .await;
    assert_eq!(
        ids(&stops.body),
        ["urn:ngsi-ld:Stop:1"],
        "a type with no temporal slot matches whole"
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn next_token_bound_to_view_and_query() -> Result<(), String> {
    let f = fixture().await?;
    let mut seen = Vec::new();
    let mut uri = format!("{CARS}?bbox=17,48,180,49&limit=1");
    let mut pages = 0;
    loop {
        let a = get(&f.app, &uri).await;
        assert_eq!(a.status, StatusCode::OK, "{uri}: {}", a.body);
        assert!(a.body.get("numberReturned") == Some(&json!(1)) || pages > 0);
        seen.extend(ids(&a.body));
        pages += 1;
        match href(&a.body, "next") {
            Some(next) => uri = local(&next),
            None => break,
        }
        assert!(pages < 10, "the pages end");
    }
    seen.sort();
    assert_eq!(
        seen,
        ["urn:ngsi-ld:Car:1", "urn:ngsi-ld:Car:3"],
        "the box part, then the part without geometry"
    );
    let first = get(&f.app, &format!("{CARS}?limit=1")).await;
    let token = href(&first.body, "next")
        .and_then(|h| h.split("next=").nth(1).map(str::to_owned))
        .ok_or("a token")?;
    for other in [
        format!("/x/views/slow/ogc/features/collections/Car/items?limit=1&next={token}"),
        format!("{CARS}?limit=2&next={token}"),
        format!("{CARS}?limit=1&next={token}x"),
    ] {
        assert_eq!(
            get(&f.app, &other).await.status,
            StatusCode::BAD_REQUEST,
            "{other}"
        );
    }
    assert_eq!(
        get(&f.app, &format!("{CARS}?limit=1&next={token}"))
            .await
            .status,
        StatusCode::OK
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn counts_are_the_view_counts() -> Result<(), String> {
    let f = fixture().await?;
    let a = get(&f.app, &format!("{CARS}?bbox=179,-1,-179,1&limit=2")).await;
    assert_eq!(
        (a.body.get("numberMatched"), a.body.get("numberReturned")),
        (Some(&json!(3)), Some(&json!(2)))
    );
    assert_eq!(a.body.get("type"), Some(&json!("FeatureCollection")));
    assert!(href(&a.body, "self").is_some_and(|s| s.ends_with("?bbox=179,-1,-179,1&limit=2")));
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn feature_of_another_type_is_404() -> Result<(), String> {
    let f = fixture().await?;
    let car = get(&f.app, &format!("{CARS}/urn:ngsi-ld:Car:1")).await;
    assert_eq!(car.status, StatusCode::OK, "{}", car.body);
    assert_eq!(car.body.get("id"), Some(&json!("urn:ngsi-ld:Car:1")));
    let encoded = get(&f.app, &format!("{CARS}/urn%3Angsi-ld%3ACar%3A1")).await;
    assert_eq!(
        encoded.body, car.body,
        "a percent-encoded id is the same Feature"
    );
    let stop = get(&f.app, &format!("{CARS}/urn:ngsi-ld:Stop:1")).await;
    let absent = get(&f.app, &format!("{CARS}/urn:ngsi-ld:Car:99")).await;
    let depot = get(&f.app, &format!("{CARS}/urn:ngsi-ld:Depot:1")).await;
    for other in [&stop, &absent, &depot] {
        assert_eq!(
            (other.status, &other.body),
            (StatusCode::NOT_FOUND, &absent.body)
        );
    }
    let bad = get(&f.app, &format!("{CARS}/a%zz")).await;
    assert_eq!(bad.status, StatusCode::NOT_FOUND);
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn an_empty_collection_is_an_empty_feature_collection() -> Result<(), String> {
    let f = fixture().await?;
    // `slow` narrows to speed<50, which no Stop has.
    let a = get(&f.app, "/x/views/slow/ogc/features/collections/Stop/items").await;
    assert_eq!(a.status, StatusCode::OK, "{}", a.body);
    assert_eq!(
        (a.body.get("numberMatched"), a.body.get("features")),
        (Some(&json!(0)), Some(&json!([])))
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn an_id_that_is_no_uri_is_404_and_a_view_without_queries_refuses() -> Result<(), String> {
    let f = fixture().await?;
    for id in ["abc", "not%20a%20urn"] {
        let a = get(&f.app, &format!("{CARS}/{id}")).await;
        assert_eq!(a.status, StatusCode::NOT_FOUND, "{id}: {}", a.body);
    }
    let reads = "/x/views/reads/ogc/features/collections/Car/items";
    for path in [reads.to_owned(), format!("{reads}/urn:ngsi-ld:Car:1")] {
        let a = get(&f.app, &path).await;
        assert_eq!(
            (a.status, a.body.get("detail")),
            (
                StatusCode::METHOD_NOT_ALLOWED,
                Some(&json!("the view refused the query"))
            ),
            "{path}"
        );
    }
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn a_next_link_crosses_from_the_bbox_part_to_the_features_without_geometry(
) -> Result<(), String> {
    let f = fixture().await?;
    let first = get(&f.app, &format!("{CARS}?bbox=170,-1,-170,1&limit=2")).await;
    assert_eq!(ids(&first.body), ["urn:ngsi-ld:Car:2", "urn:ngsi-ld:Car:4"]);
    assert_eq!(first.body.get("numberMatched"), Some(&json!(3)));
    let next = href(&first.body, "next").ok_or("no next link")?;
    let second = get(&f.app, &local(&next)).await;
    assert_eq!(ids(&second.body), ["urn:ngsi-ld:Car:3"], "{}", second.body);
    assert_eq!(href(&second.body, "next"), None);
    let one = get(&f.app, &format!("{CARS}?bbox=170,-1,-170,1&limit=1")).await;
    assert_eq!(ids(&one.body), ["urn:ngsi-ld:Car:2"]);
    let next = href(&one.body, "next").ok_or("no next link")?;
    let rest = get(&f.app, &local(&next)).await;
    assert_eq!(ids(&rest.body), ["urn:ngsi-ld:Car:4"], "{}", rest.body);
    Ok(())
}

/// T-3795: a token into the part of the Features without a geometry keeps its offset, so
/// more of them than a page are read to their end, each once.
#[tokio::test(flavor = "multi_thread")]
async fn the_features_without_geometry_page_to_their_end() -> Result<(), String> {
    let f = fixture().await?;
    let car = json!({"id": "urn:ngsi-ld:Car:5", "type": "Car", "speed": {"type": "Property", "value": 5}});
    let a = send(
        &f.app,
        "POST",
        "/ngsi-ld/v1/entities",
        &car.to_string(),
        &[("NGSILD-Tenant", "city")],
    )
    .await;
    assert_eq!(a.status, StatusCode::CREATED, "{}", a.body);
    let mut uri = format!("{CARS}?bbox=0,0,1,1&limit=1");
    let mut seen = Vec::new();
    for _ in 0..5 {
        let page = get(&f.app, &uri).await;
        assert_eq!(page.status, StatusCode::OK, "{}", page.body);
        assert_eq!(
            page.body.get("numberMatched"),
            Some(&json!(2)),
            "{}",
            page.body
        );
        seen.extend(ids(&page.body));
        match href(&page.body, "next") {
            Some(next) => uri = local(&next),
            None => break,
        }
    }
    assert_eq!(seen, ["urn:ngsi-ld:Car:3", "urn:ngsi-ld:Car:5"]);
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn a_tenant_that_holds_nothing_is_an_empty_collection() -> Result<(), String> {
    let f = fixture().await?;
    let schema = send(&f.app, "PUT", "/q/schemas/nobody", SCHEMA, &[]).await;
    assert_eq!(schema.status, StatusCode::CREATED, "{}", schema.body);
    let view = json!({"id": "ghost", "tenant": "nobody", "selector": ["Car"],
                      "rules": [{"types": ["Car"], "attributes": ["speed"]}],
                      "operations": ["query_entities"]});
    let put = send(&f.app, "PUT", "/q/views/ghost", &view.to_string(), &[]).await;
    assert_eq!(put.status, StatusCode::CREATED, "{}", put.body);
    let a = get(&f.app, "/x/views/ghost/ogc/features/collections/Car/items").await;
    assert_eq!(
        (
            a.status,
            a.body.get("numberMatched"),
            a.body.get("features")
        ),
        (StatusCode::OK, Some(&json!(0)), Some(&json!([])))
    );
    Ok(())
}

/// The `href` of each link of `rel` on a Feature.
fn feature_links(feature: &Value, rel: &str) -> Vec<String> {
    feature
        .get("links")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter(|l| l.get("rel").and_then(Value::as_str) == Some(rel))
        .filter_map(|l| l.get("href").and_then(Value::as_str))
        .map(str::to_owned)
        .collect()
}

#[tokio::test(flavor = "multi_thread")]
async fn a_feature_links_its_entity_and_its_schema() -> Result<(), String> {
    let f = fixture().await?;
    let view = json!({"id": "linked", "tenant": "city", "selector": ["Car"],
                      "rules": [{"types": ["Car"], "attributes": ["speed"]}],
                      "operations": ["query_entities", "retrieve_entity"]});
    let put = send(&f.app, "PUT", "/q/views/linked", &view.to_string(), &[]).await;
    assert_eq!(put.status, StatusCode::CREATED, "{}", put.body);
    let root = "http://localhost:9090/x/views";
    let page = get(&f.app, "/x/views/linked/ogc/features/collections/Car/items").await;
    let features = page.body.get("features").and_then(Value::as_array).cloned();
    let features = features.ok_or("no features")?;
    assert_eq!(features.len(), 4, "{}", page.body);
    for feature in &features {
        let urn = feature.get("id").and_then(Value::as_str).ok_or("no id")?;
        let encoded = urn.replace(':', "%3A");
        assert_eq!(
            feature_links(feature, "alternate"),
            [format!("{root}/linked/ngsi-ld/v1/entities/{encoded}")],
            "EP-38: the Entity under the same view"
        );
        assert_eq!(
            feature_links(feature, "describedby"),
            [format!("{root}/linked/ogc/features/collections/Car/schema")]
        );
    }
    let one = get(
        &f.app,
        "/x/views/linked/ogc/features/collections/Car/items/urn:ngsi-ld:Car:1",
    )
    .await;
    assert_eq!(one.status, StatusCode::OK, "{}", one.body);
    assert_eq!(
        feature_links(&one.body, "alternate"),
        [format!(
            "{root}/linked/ngsi-ld/v1/entities/urn%3Angsi-ld%3ACar%3A1"
        )]
    );
    assert_eq!(
        feature_links(&one.body, "self").len(),
        1,
        "the item keeps its own links"
    );
    let schema = get(
        &f.app,
        &local(&feature_links(&one.body, "describedby").concat()),
    )
    .await;
    assert_eq!(schema.status, StatusCode::OK, "the schema link answers");
    let city = get(&f.app, CARS).await;
    let any_alternate = city
        .body
        .get("features")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .any(|f| !feature_links(f, "alternate").is_empty());
    assert!(
        !any_alternate,
        "a view without Retrieve Entity links no Entity it would refuse"
    );
    Ok(())
}
