// SPDX-License-Identifier: EUPL-1.2
//! What the view root answers when it does not serve (surfaces-test-plan
//! section 6, G9; ADR-0023 rule 4): no error names a hidden member or
//! carries an inner failure's detail, and a request the view does not
//! serve gets 405 with `Allow`, 422 when the method stays, 404 where the
//! view serves nothing.

use crate::common::{fixture, get, put_view, send, CARS};
use antares_ops::INNER_DETAIL;
use axum::http::{Method, StatusCode};
use serde_json::json;

#[tokio::test(flavor = "multi_thread")]
async fn views_errors_carry_no_hidden_name_and_no_inner_detail() -> Result<(), String> {
    let f = fixture().await?;
    for uri in [
        "/entities/urn:ngsi-ld:Car:3",
        "/entities/urn:ngsi-ld:Org:o",
        "/entities/urn:ngsi-ld:Car:1/attrs/nothere",
        "/types/Nothing",
        "/attributes/nothere",
        "/entities?type=Car&q=speed%3D%3D",
    ] {
        let a = get(&f.app, &format!("{CARS}{uri}")).await;
        assert!(a.status.is_client_error(), "{uri}: {}", a.text);
        for hidden in [
            "secret", "salary", "Acme", "4321", "Car:3\"", "Org\"", "speed<80",
        ] {
            assert!(!a.text.contains(hidden), "{uri} names {hidden}: {}", a.text);
        }
    }
    let ctx = "http://127.0.0.1:9/nowhere-7f3a.jsonld";
    let link = format!(
        "<{ctx}>; rel=\"http://www.w3.org/ns/json-ld#context\"; type=\"application/ld+json\""
    );
    let a = send(
        &f.app,
        Method::GET,
        &format!("{CARS}/entities?type=Car"),
        &[("Link", &link)],
        None,
    )
    .await;
    assert_eq!(a.status, StatusCode::INTERNAL_SERVER_ERROR, "{}", a.text);
    assert!(a.text.contains(INNER_DETAIL), "{}", a.text);
    assert!(
        !a.text.contains("nowhere-7f3a") && !a.text.contains("127.0.0.1"),
        "{}",
        a.text
    );
    Ok(())
}

/// Rule 4 for the reads: a write on a resource the view reads is 405 with
/// the methods it serves there; HEAD follows GET.
#[tokio::test(flavor = "multi_thread")]
async fn views_a_disallowed_operation_answers_405_with_allow() -> Result<(), String> {
    let f = fixture().await?;
    let car = json!({"id": "urn:ngsi-ld:Car:7", "type": "Car", "speed": {"type": "Property", "value": 1}});
    for (method, uri, allow) in [
        (Method::POST, "/entities", "GET, HEAD"),
        (Method::DELETE, "/entities/urn:ngsi-ld:Car:1", "GET, HEAD"),
        (
            Method::PATCH,
            "/entities/urn:ngsi-ld:Car:1/attrs/speed",
            "GET, HEAD",
        ),
    ] {
        let a = send(
            &f.app,
            method.clone(),
            &format!("{CARS}{uri}"),
            &[],
            Some(&car),
        )
        .await;
        assert_eq!(
            a.status,
            StatusCode::METHOD_NOT_ALLOWED,
            "{method} {uri}: {}",
            a.text
        );
        assert_eq!(a.header("allow").as_deref(), Some(allow), "{method} {uri}");
    }
    let made = send(
        &f.app,
        Method::GET,
        "/ngsi-ld/v1/entities/urn:ngsi-ld:Car:7",
        &[("NGSILD-Tenant", "city")],
        None,
    )
    .await;
    assert_eq!(
        made.status,
        StatusCode::NOT_FOUND,
        "nothing was written: {}",
        made.text
    );
    let head = send(
        &f.app,
        Method::HEAD,
        &format!("{CARS}/entities?type=Car"),
        &[],
        None,
    )
    .await;
    assert_eq!(head.status, StatusCode::OK);
    assert_eq!(head.text, "", "HEAD carries no body");
    Ok(())
}

/// Rule 4: one method and path, two operations, the view allows one: the
/// other is 422; a resource the view serves nothing on is 404, as is a path
/// that is no NGSI-LD resource and an unknown view.
#[tokio::test(flavor = "multi_thread")]
async fn views_a_disallowed_operation_answers_422_when_the_method_stays() -> Result<(), String> {
    let f = fixture().await?;
    put_view(
        &f.app,
        &json!({
            "id": "plain", "tenant": "city", "selector": ["Car"],
            "rules": [{"types": ["Car"], "attributes": ["speed"]}],
            "operations": ["retrieve_entity_types", "retrieve_attributes"]
        }),
    )
    .await?;
    let plain = "/x/views/plain/ngsi-ld/v1";
    let a = get(&f.app, &format!("{plain}/types")).await;
    assert_eq!(a.status, StatusCode::OK, "{}", a.text);
    let head = send(
        &f.app,
        Method::HEAD,
        &format!("{plain}/types?details=true"),
        &[],
        None,
    )
    .await;
    assert_eq!(
        head.status,
        StatusCode::UNPROCESSABLE_ENTITY,
        "HEAD follows GET"
    );
    for uri in ["/types?details=true", "/attributes?details=true"] {
        let a = get(&f.app, &format!("{plain}{uri}")).await;
        assert_eq!(
            a.status,
            StatusCode::UNPROCESSABLE_ENTITY,
            "{uri}: {}",
            a.text
        );
        assert_eq!(
            a.json.get("title"),
            Some(&json!("OperationNotSupported")),
            "{}",
            a.text
        );
    }
    for uri in [
        format!("{plain}/entities?type=Car"),
        format!("{plain}/subscriptions"),
        format!("{CARS}/entityOperations/create"),
        format!("{plain}/nowhere"),
        format!("{CARS}/jsonldContexts/anything"),
        "/x/views/nosuch/ngsi-ld/v1/types".to_owned(),
        "/x/views/Plain/ngsi-ld/v1/types".to_owned(),
    ] {
        let a = get(&f.app, &uri).await;
        assert_eq!(a.status, StatusCode::NOT_FOUND, "{uri}: {}", a.text);
        assert_eq!(
            a.json.get("title"),
            Some(&json!("ResourceNotFound")),
            "{uri}: {}",
            a.text
        );
    }
    Ok(())
}
