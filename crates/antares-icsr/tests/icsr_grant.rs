// SPDX-License-Identifier: EUPL-1.2
//! ADR-0025 rule 5, threats C1, C6, C7 and C8: an internal forward into a
//! Tenant needs that Tenant's export grant, checked when the registration
//! is stored and on every forward; a write needs `read-write`; the broker's
//! own Tenants are never a target.

mod common;

use common::{broker, car, cars, grant, ids, register, send, warned};
use serde_json::json;

/// C1: no grant, no registration (403 before it is stored, at 12.2.2 and
/// 12.2.4); with a grant the registration is stored and B's Cars arrive.
#[tokio::test]
async fn icsr_a_registration_into_a_tenant_that_granted_nothing_is_refused() {
    let (_b, app) = broker(None).await;
    car(&app, "b", 1).await;
    let refused = register(&app, "a", "r1", Some("b"), json!({})).await;
    assert_eq!(refused.status, 403, "{}", refused.body);
    assert!(
        refused.body.to_string().contains("export grant"),
        "{}",
        refused.body
    );
    let listed = send(
        &app,
        "GET",
        "/ngsi-ld/v1/csourceRegistrations",
        Some("a"),
        None,
        None,
    )
    .await;
    assert_eq!(
        ids(&listed.body),
        Vec::<String>::new(),
        "nothing was stored"
    );

    grant(&app, "b", "a", json!({})).await;
    let stored = register(&app, "a", "r1", Some("b"), json!({})).await;
    assert_eq!(stored.status, 201, "{}", stored.body);
    let read = cars(&app, "a", None).await;
    assert_eq!(read.status, 200, "{}", read.body);
    assert_eq!(ids(&read.body), ["urn:ngsi-ld:Car:b-1"]);
    assert!(read.warnings.is_empty(), "{:?}", read.warnings);

    // 12.2.4: moving it to a Tenant that granted nothing is refused, and
    // the stored one is unchanged.
    let moved = send(
        &app,
        "PATCH",
        "/ngsi-ld/v1/csourceRegistrations/urn:ngsi-ld:ContextSourceRegistration:r1",
        Some("a"),
        None,
        Some(json!({"tenant": "c"})),
    )
    .await;
    assert_eq!(moved.status, 403, "{}", moved.body);
    assert_eq!(
        ids(&cars(&app, "a", None).await.body),
        ["urn:ngsi-ld:Car:b-1"]
    );
}

/// C1, C7: a removed grant stops the next forward at once (299, no data),
/// and one written again opens it. Notifications: T-3700.
#[tokio::test]
async fn icsr_a_revoked_grant_stops_forwards() {
    let (_b, app) = broker(None).await;
    car(&app, "b", 1).await;
    grant(&app, "b", "a", json!({})).await;
    assert_eq!(
        register(&app, "a", "r1", Some("b"), json!({})).await.status,
        201
    );
    assert_eq!(ids(&cars(&app, "a", None).await.body).len(), 1);

    let removed = send(&app, "DELETE", "/q/grants/b/a", None, None, None).await;
    assert_eq!(removed.status, 204, "{}", removed.body);
    let after = cars(&app, "a", None).await;
    assert_eq!(after.status, 200, "{}", after.body);
    assert!(ids(&after.body).is_empty(), "{}", after.body);
    assert!(warned(&after.warnings, "299"), "{:?}", after.warnings);

    grant(&app, "b", "a", json!({})).await;
    assert_eq!(ids(&cars(&app, "a", None).await.body).len(), 1);
}

/// C6: a forwarded write needs `read-write`; on a read grant it is refused
/// like an engine refusal and nothing lands in B.
#[tokio::test]
async fn icsr_a_write_forward_needs_a_read_write_grant() {
    let (_b, app) = broker(None).await;
    grant(&app, "b", "a", json!({})).await;
    let reg = register(
        &app,
        "a",
        "w",
        Some("b"),
        json!({"operations": ["createEntity", "queryEntity"]}),
    )
    .await;
    assert_eq!(reg.status, 201, "{}", reg.body);
    let new_car = |n: u32| json!({"id": format!("urn:ngsi-ld:Car:w{n}"), "type": "Car"});
    let refused = send(
        &app,
        "POST",
        "/ngsi-ld/v1/entities",
        Some("a"),
        None,
        Some(new_car(1)),
    )
    .await;
    assert_eq!(refused.status, 207, "{}", refused.body);
    assert!(
        ids(&cars(&app, "b", None).await.body).is_empty(),
        "nothing lands in b"
    );

    grant(&app, "b", "a", json!({"access": "read-write"})).await;
    let made = send(
        &app,
        "POST",
        "/ngsi-ld/v1/entities",
        Some("a"),
        None,
        Some(new_car(2)),
    )
    .await;
    assert_eq!(made.status, 201, "{}", made.body);
    assert!(made.warnings.is_empty(), "{:?}", made.warnings);
    assert_eq!(
        ids(&cars(&app, "b", None).await.body),
        ["urn:ngsi-ld:Car:w2"]
    );
}

/// C8: no grant names one of the broker's own Tenants, so none is ever a
/// target, whatever a registration says.
#[tokio::test]
async fn icsr_an_internal_tenant_is_not_a_target() {
    let (_b, app) = broker(None).await;
    for target in ["antares-views", "antares-grants", "snap-1", "distsub-index"] {
        let path = format!("/q/grants/{target}/a");
        let doc = json!({"target": target, "source": "a"});
        let a = send(&app, "PUT", &path, None, None, Some(doc)).await;
        assert_eq!(a.status, 400, "{path}: {}", a.body);
        let r = register(&app, "a", "i", Some(target), json!({})).await;
        assert!(
            matches!(r.status, 400 | 403),
            "{target}: {} {}",
            r.status,
            r.body
        );
    }
    let listed = send(&app, "GET", "/q/grants", None, None, None).await;
    assert_eq!(listed.body, json!([]));
}

/// A grant's view (ADR-0023) narrows what B exports on top of B's engine:
/// only the Attributes it shows arrive, and a view of another Tenant
/// exports nothing.
#[tokio::test]
async fn icsr_a_grant_view_narrows_what_the_target_exports() {
    let (_b, app) = broker(None).await;
    car(&app, "b", 1).await;
    for (id, tenant) in [("b-speed", "b"), ("c-speed", "c")] {
        let view = json!({"id": id, "tenant": tenant, "selector": ["Car"],
                          "rules": [{"types": ["Car"], "attributes": ["speed"]}],
                          "operations": ["query_entities", "retrieve_entity"]});
        let put = send(
            &app,
            "PUT",
            &format!("/q/views/{id}"),
            None,
            None,
            Some(view),
        )
        .await;
        assert_eq!(put.status, 201, "{}", put.body);
    }
    grant(&app, "b", "a", json!({"view": "b-speed"})).await;
    assert_eq!(
        register(&app, "a", "r1", Some("b"), json!({})).await.status,
        201
    );
    let read = cars(&app, "a", None).await;
    assert_eq!(ids(&read.body), ["urn:ngsi-ld:Car:b-1"], "{}", read.body);
    let first = read.body.get(0).cloned().unwrap_or_default();
    assert!(
        first.get("speed").is_some() && first.get("owner").is_none(),
        "{first}"
    );

    grant(&app, "b", "a", json!({"view": "c-speed"})).await;
    let other = cars(&app, "a", None).await;
    assert!(ids(&other.body).is_empty(), "{}", other.body);
    assert!(warned(&other.warnings, "299"), "{:?}", other.warnings);
}

/// `/q/grants` refuses a document that is no grant or names another pair
/// than its path, and answers 404 for a pair with none.
#[tokio::test]
async fn the_grant_routes_check_what_they_store() {
    let (_b, app) = broker(None).await;
    for (path, doc) in [
        ("/q/grants/b/a", json!({"target": "b", "source": "c"})),
        (
            "/q/grants/b/a",
            json!({"target": "b", "source": "a", "access": "all"}),
        ),
        (
            "/q/grants/b/a",
            json!({"target": "b", "source": "a", "view": "No View"}),
        ),
    ] {
        let a = send(&app, "PUT", path, None, None, Some(doc.clone())).await;
        assert_eq!(a.status, 400, "{doc}: {}", a.body);
    }
    assert_eq!(
        send(&app, "GET", "/q/grants/b/a", None, None, None)
            .await
            .status,
        404
    );
    assert_eq!(
        send(&app, "DELETE", "/q/grants/b/a", None, None, None)
            .await
            .status,
        404
    );
    grant(&app, "b", "a", json!({"access": "read-write"})).await;
    let one = send(&app, "GET", "/q/grants/b/a", None, None, None).await;
    assert_eq!(
        one.body,
        json!({"target": "b", "source": "a", "access": "read-write"})
    );
    grant(&app, "b", "a", json!({})).await;
    let all = send(&app, "GET", "/q/grants", None, None, None).await;
    assert_eq!(
        all.body,
        json!([{"target": "b", "source": "a", "access": "read"}])
    );
}
