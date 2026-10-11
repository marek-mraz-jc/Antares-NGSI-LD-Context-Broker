// SPDX-License-Identifier: EUPL-1.2
//! Part 1 Core: landing page, conformance, HTTP (mapping rows C1, C3, C4, C9, C21, C22).

use crate::common::{fixture, get, href, send};
use axum::http::StatusCode;
use serde_json::json;

const ROOT: &str = "/x/views/city/ogc/features";

#[tokio::test(flavor = "multi_thread")]
async fn landing_page_links() -> Result<(), String> {
    let f = fixture().await?;
    for root in [ROOT.to_owned(), format!("{ROOT}/")] {
        let a = get(&f.app, &root).await;
        assert_eq!(a.status, StatusCode::OK, "{root}: {}", a.body);
        assert_eq!(
            a.headers
                .get("content-type")
                .map(axum::http::HeaderValue::as_bytes),
            Some(&b"application/json"[..])
        );
        for (rel, tail) in [
            ("self", ""),
            ("service-desc", "/api"),
            ("conformance", "/conformance"),
            ("data", "/collections"),
        ] {
            assert_eq!(
                href(&a.body, rel),
                Some(format!("http://localhost:9090{ROOT}{tail}")),
                "{rel}"
            );
        }
        let links = a
            .body
            .get("links")
            .and_then(|l| l.as_array())
            .cloned()
            .unwrap_or_default();
        assert!(
            links.iter().all(|l| l.get("type").is_some()),
            "every link has a type (Req 29)"
        );
    }
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn conformance_lists_what_is_served() -> Result<(), String> {
    let f = fixture().await?;
    let a = get(&f.app, &format!("{ROOT}/conformance")).await;
    assert_eq!(a.status, StatusCode::OK);
    assert_eq!(a.body, json!({"conformsTo": antares_ogc::CONFORMANCE}));
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn head_is_the_get_without_its_body() -> Result<(), String> {
    let f = fixture().await?;
    for path in [
        "",
        "/conformance",
        "/api",
        "/collections",
        "/collections/Car",
        "/collections/Car/items",
        "/collections/Car/items/urn:ngsi-ld:Car:1",
        "/collections/Nope",
    ] {
        let uri = format!("{ROOT}{path}");
        let got = get(&f.app, &uri).await;
        let head = send(&f.app, "HEAD", &uri, "", &[]).await;
        assert_eq!(head.status, got.status, "{uri}");
        assert_eq!(head.bytes, 0, "{uri}");
        let lengths: Vec<_> = head.headers.get_all("content-length").iter().collect();
        assert_eq!(lengths.len(), 1, "{uri}: one Content-Length");
        assert_eq!(
            lengths.first().and_then(|v| v.to_str().ok()),
            Some(got.bytes.to_string().as_str()),
            "{uri}"
        );
    }
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn unknown_and_invalid_parameters_are_400() -> Result<(), String> {
    let f = fixture().await?;
    for uri in [
        format!("{ROOT}?f=json"),
        format!("{ROOT}/conformance?x=1"),
        format!("{ROOT}/collections?limit=1"),
        format!("{ROOT}/collections/Car?bbox=0,0,1,1"),
        format!("{ROOT}/collections/Car/items/urn:ngsi-ld:Car:1?limit=1"),
        format!("{ROOT}/collections/Car/items?speed=10"),
        format!("{ROOT}/collections/Car/items?crs=http://www.opengis.net/def/crs/EPSG/0/4326"),
        format!("{ROOT}/collections/Car/items?limit=0"),
        format!("{ROOT}/collections/Car/items?limit=10001"),
        format!("{ROOT}/collections/Car/items?bbox=1,2,3"),
        format!("{ROOT}/collections/Car/items?datetime=yesterday"),
        format!("{ROOT}/collections/Car/items?limit=1&limit=2"),
        format!("{ROOT}/collections/Car/items?next=garbage"),
    ] {
        let a = get(&f.app, &uri).await;
        assert_eq!(a.status, StatusCode::BAD_REQUEST, "{uri}: {}", a.body);
        assert_eq!(
            a.headers
                .get("content-type")
                .map(axum::http::HeaderValue::as_bytes),
            Some(&b"application/problem+json"[..]),
            "{uri}"
        );
        assert_eq!(a.body.get("status"), Some(&json!(400)), "{uri}");
    }
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn html_only_accept_is_406() -> Result<(), String> {
    let f = fixture().await?;
    let a = send(&f.app, "GET", ROOT, "", &[("Accept", "text/html")]).await;
    assert_eq!(a.status, StatusCode::NOT_ACCEPTABLE, "{}", a.body);
    let a = send(
        &f.app,
        "GET",
        ROOT,
        "",
        &[("Accept", "text/html, application/json;q=0.5")],
    )
    .await;
    assert_eq!(a.status, StatusCode::OK, "JSON among the ranges is served");
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn gml_only_accept_is_406() -> Result<(), String> {
    let f = fixture().await?;
    let a = send(
        &f.app,
        "GET",
        &format!("{ROOT}/collections/Car/items"),
        "",
        &[("Accept", "application/gml+xml")],
    )
    .await;
    assert_eq!(a.status, StatusCode::NOT_ACCEPTABLE, "{}", a.body);
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn an_unknown_view_a_view_without_schema_and_an_unknown_path_are_404() -> Result<(), String> {
    let f = fixture().await?;
    for uri in [
        "/x/views/nope/ogc/features".to_owned(),
        format!("{ROOT}/elsewhere"),
        format!("{ROOT}/collections/Car/items/a/b"),
    ] {
        assert_eq!(
            get(&f.app, &uri).await.status,
            StatusCode::NOT_FOUND,
            "{uri}"
        );
    }
    let other = json!({"id": "plain", "tenant": "other", "selector": ["Car"], "rules": [{"types": ["Car"], "attributes": ["speed"]}], "operations": ["query_entities"]});
    assert_eq!(
        send(&f.app, "PUT", "/q/views/plain", &other.to_string(), &[])
            .await
            .status,
        StatusCode::CREATED
    );
    assert_eq!(
        get(&f.app, "/x/views/plain/ogc/features").await.status,
        StatusCode::NOT_FOUND,
        "the Tenant has no schema"
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn a_write_method_is_405_with_allow() -> Result<(), String> {
    let f = fixture().await?;
    for method in ["POST", "PUT", "PATCH", "DELETE"] {
        let a = send(
            &f.app,
            method,
            &format!("{ROOT}/collections/Car/items"),
            "{}",
            &[],
        )
        .await;
        assert_eq!(a.status, StatusCode::METHOD_NOT_ALLOWED, "{method}");
        assert_eq!(
            a.headers
                .get("allow")
                .map(axum::http::HeaderValue::as_bytes),
            Some(&b"GET, HEAD"[..]),
            "{method}"
        );
    }
    Ok(())
}
