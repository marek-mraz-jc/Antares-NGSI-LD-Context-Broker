// SPDX-License-Identifier: EUPL-1.2
//! The engine through the broker's own router: the security cases of the
//! platform's `pdp_tests`, `write_guard_tests`, `scope_folding_tests` and
//! `filter_oracle_tests`, carried over (T-3637), each named after the
//! platform test it carries.

use antares_api::policy::{run_policy_contract, PolicyEngine};
use antares_api::AppState;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use http_body_util::BodyExt;
use joinedcontext_policy::JoinedcontextPolicy;
use serde_json::{json, Value};
use std::sync::Arc;
use tower::ServiceExt;

const AREA: &str = "georel=within;geometry=Polygon;coordinates=[[[0,0],[1,0],[1,1],[0,1],[0,0]]]";

type Res = Result<(), String>;

/// The broker with the engine, the subject headers named as the
/// deployment names them, and `doc` loaded for the default Tenant.
async fn broker(doc: Value) -> Result<(AppState, Arc<JoinedcontextPolicy>), String> {
    static HEADERS: std::sync::Once = std::sync::Once::new();
    HEADERS.call_once(|| {
        std::env::set_var(
            "ANTARES_POLICY_SUBJECT_HEADERS",
            joinedcontext_policy::subject::HEADERS.join(","),
        );
    });
    let engine = Arc::new(JoinedcontextPolicy::new());
    engine.load("default", &doc)?;
    let mut st = AppState::new("me".into()).with_policy(engine.clone());
    antares_api::wire(&mut st).await;
    Ok((st, engine))
}

async fn send(
    st: &AppState,
    method: &str,
    uri: &str,
    who: &[(&str, &str)],
    body: Option<&Value>,
) -> Result<(StatusCode, Value), String> {
    exchange(st, method, uri, who, body)
        .await
        .map(|(status, _, body)| (status, body))
}

/// `send`, keeping the answer's headers.
async fn exchange(
    st: &AppState,
    method: &str,
    uri: &str,
    who: &[(&str, &str)],
    body: Option<&Value>,
) -> Result<(StatusCode, axum::http::HeaderMap, Value), String> {
    let mut b = Request::builder().method(method).uri(uri);
    for (k, v) in who {
        b = b.header(*k, *v);
    }
    let payload = body.map(Value::to_string).unwrap_or_default();
    if body.is_some() {
        b = b
            .header("Content-Type", "application/json")
            .header("Content-Length", payload.len());
    }
    let req = b.body(Body::from(payload)).map_err(|e| e.to_string())?;
    let resp = antares_api::router(st.clone())
        .oneshot(req)
        .await
        .map_err(|e| e.to_string())?;
    let status = resp.status();
    let headers = resp.headers().clone();
    let bytes = resp
        .into_body()
        .collect()
        .await
        .map_err(|e| e.to_string())?
        .to_bytes();
    Ok((
        status,
        headers,
        serde_json::from_slice(&bytes).unwrap_or(Value::Null),
    ))
}

const ADMIN: [(&str, &str); 2] = [
    ("x-jc-subject", "admin@example.org"),
    ("x-jc-roles", "admin"),
];
const ANA: [(&str, &str); 2] = [("x-jc-subject", "ana@example.org"), ("x-jc-groups", "city")];
const ANONYMOUS: [(&str, &str); 1] = [("x-jc-roles", "")];

/// The admin may do everything: how the tests seed.
fn admin() -> Value {
    json!({"id": "admin", "assignee": {"kind": "role", "id": "admin"},
           "operations": ["5.6.1", "5.6.2", "5.6.3", "5.6.6", "5.7.1", "5.7.2"]})
}

fn doc(policies: &[Value]) -> Value {
    let mut all = vec![admin()];
    all.extend_from_slice(policies);
    json!({ "policies": all })
}

fn prop(v: Value) -> Value {
    with(json!({"type": "Property"}), "value", v)
}

async fn seed(st: &AppState, entities: &[Value]) -> Res {
    for e in entities {
        let (code, body) = send(st, "POST", "/ngsi-ld/v1/entities", &ADMIN, Some(e)).await?;
        if code != StatusCode::CREATED {
            return Err(format!("seed {e}: {code} {body}"));
        }
    }
    Ok(())
}

fn car(n: u32, speed: u32, scope: &str) -> Value {
    json!({"id": format!("urn:ngsi-ld:Car:{n}"), "type": "Car", "speed": prop(json!(speed)),
           "colour": prop(json!("red")), "scope": scope})
}

async fn fleet(st: &AppState) -> Res {
    seed(
        st,
        &[
            car(1, 10, "/geo/SK/BB"),
            car(2, 90, "/geo/SK/BB/Sasova"),
            car(3, 30, "/geo/SK/BBB"),
            json!({"id": "urn:ngsi-ld:Secret:1", "type": "Secret", "code": prop(json!("x"))}),
        ],
    )
    .await
}

/// The ids of a list answer, sorted.
fn ids(list: &Value) -> Vec<String> {
    let mut out: Vec<String> = list
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|e| e.get("id").and_then(Value::as_str).map(str::to_owned))
        .collect();
    out.sort();
    out
}

fn car_ids(ns: &[u32]) -> Vec<String> {
    ns.iter().map(|n| format!("urn:ngsi-ld:Car:{n}")).collect()
}

fn grant(id: &str, kind: &str, who: &str, ops: &[&str], information: Value) -> Value {
    let p = json!({"id": id, "assignee": {"kind": kind, "id": who}, "operations": ops});
    with(p, "information", information)
}

fn cars(attrs: &[&str]) -> Value {
    json!([{"entities": [{"type": "Car"}], "propertyNames": attrs}])
}

fn with(mut p: Value, k: &str, v: Value) -> Value {
    if let Some(o) = p.as_object_mut() {
        o.insert(k.to_owned(), v);
    }
    p
}

const QUERY: &str = "/ngsi-ld/v1/entities?type=Car,Secret";

#[tokio::test]
async fn the_engine_holds_the_policy_contract() {
    run_policy_contract(&JoinedcontextPolicy::new()).await;
}

#[tokio::test]
async fn an_empty_policy_set_denies() -> Res {
    let (st, engine) = broker(doc(&[])).await?;
    fleet(&st).await?;
    assert_eq!(
        send(&st, "GET", QUERY, &ANA, None).await?.0,
        StatusCode::FORBIDDEN
    );
    engine.load("default", &Value::Null)?;
    assert_eq!(
        send(&st, "GET", QUERY, &ADMIN, None).await?.0,
        StatusCode::FORBIDDEN
    );
    Ok(())
}

#[tokio::test]
async fn an_anonymous_caller_gets_the_public_grant_as_a_rewrite() -> Res {
    let (st, _) = broker(doc(&[grant(
        "pub",
        "role",
        "public",
        &["5.7.2"],
        cars(&["speed"]),
    )]))
    .await?;
    fleet(&st).await?;
    let (code, list) = send(&st, "GET", QUERY, &ANONYMOUS, None).await?;
    assert_eq!(code, StatusCode::OK, "{list}");
    assert_eq!(ids(&list), car_ids(&[1, 2, 3]));
    assert!(list.to_string().contains("speed") && !list.to_string().contains("colour"));
    // a verified caller is not anonymous and holds no `public` it did not get
    assert_eq!(
        send(&st, "GET", QUERY, &ANA, None).await?.0,
        StatusCode::FORBIDDEN
    );
    Ok(())
}

/// K38, DS-02: a data-space participant reads what its agreement's Policy grants it, through
/// the header the gateway sets from the transfer token, and none of the public grant.
#[tokio::test]
async fn a_participant_reads_what_its_agreement_grants_and_nothing_public() -> Res {
    let did = "did:web:consumer.example";
    let (st, _) = broker(doc(&[
        grant("agreed", "did", did, &["5.7.2"], cars(&["colour"])),
        grant("pub", "role", "public", &["5.7.2"], cars(&["speed"])),
    ]))
    .await?;
    fleet(&st).await?;
    let participant = [
        ("x-jc-did", "did%3Aweb%3Aconsumer.example"),
        ("x-jc-roles", "public"),
    ];
    let (code, list) = send(&st, "GET", QUERY, &participant, None).await?;
    assert_eq!(code, StatusCode::OK, "{list}");
    assert_eq!(ids(&list), car_ids(&[1, 2, 3]));
    let text = list.to_string();
    assert!(text.contains("colour") && !text.contains("speed"), "{text}");
    let other = [("x-jc-did", "did:web:other.example")];
    assert_eq!(
        send(&st, "GET", QUERY, &other, None).await?.0,
        StatusCode::FORBIDDEN
    );
    Ok(())
}

#[tokio::test]
async fn an_anonymous_caller_gets_the_grant_of_the_endpoint_role_the_gateway_resolved() -> Res {
    let (st, _) = broker(doc(&[grant(
        "app",
        "role",
        "endpoint:p/e",
        &["5.7.2"],
        cars(&["speed"]),
    )]))
    .await?;
    fleet(&st).await?;
    // AP-97: the gateway pins the Endpoint's caller role for an anonymous caller it admits
    let admitted = [("x-jc-roles", "endpoint%3Ap%2Fe")];
    let (code, list) = send(&st, "GET", QUERY, &admitted, None).await?;
    assert_eq!(code, StatusCode::OK, "{list}");
    assert_eq!(ids(&list), car_ids(&[1, 2, 3]));
    // another Endpoint's role, or a realm role claimed beside it, grants nothing
    for roles in ["endpoint%3Ap%2Fother", "admin", "endpoint:"] {
        let other = [("x-jc-roles", roles)];
        assert_eq!(
            send(&st, "GET", QUERY, &other, None).await?.0,
            StatusCode::FORBIDDEN,
            "{roles}"
        );
    }
    Ok(())
}

#[tokio::test]
async fn a_caller_the_policy_does_not_name_is_denied() -> Res {
    let (st, _) = broker(doc(&[grant(
        "bob",
        "user",
        "bob@example.org",
        &["5.7.2"],
        cars(&[]),
    )]))
    .await?;
    fleet(&st).await?;
    let (code, body) = send(&st, "GET", QUERY, &ANA, None).await?;
    assert_eq!(code, StatusCode::FORBIDDEN);
    // a_policy_refusal_discloses_no_rule
    assert!(!body.to_string().contains("bob"), "{body}");
    let bob = [("x-jc-subject", "bob@example.org")];
    assert_eq!(send(&st, "GET", QUERY, &bob, None).await?.0, StatusCode::OK);
    Ok(())
}

#[tokio::test]
async fn an_operation_outside_the_grant_is_denied() -> Res {
    let (st, _) = broker(doc(&[grant(
        "city",
        "group",
        "city",
        &["5.7.2"],
        cars(&[]),
    )]))
    .await?;
    let (code, _) = send(
        &st,
        "POST",
        "/ngsi-ld/v1/entities",
        &ANA,
        Some(&car(9, 1, "/a")),
    )
    .await?;
    assert_eq!(code, StatusCode::FORBIDDEN);
    Ok(())
}

#[tokio::test]
async fn a_prohibition_overrides_a_permission_that_would_have_matched() -> Res {
    let deny = with(
        grant(
            "no-secret",
            "group",
            "city",
            &["5.7.2"],
            json!([{"entities": [{"type": "Secret"}]}]),
        ),
        "effect",
        json!("prohibit"),
    );
    let all = grant("all", "group", "city", &["5.7.2"], json!([]));
    let (st, _) = broker(doc(&[all, deny])).await?;
    fleet(&st).await?;
    let (code, body) = send(&st, "GET", QUERY, &ANA, None).await?;
    assert_eq!(code, StatusCode::FORBIDDEN, "{body}");
    assert!(!body.to_string().contains("no-secret"), "{body}");
    // asking for a type it does not name is not prohibited
    let (code, list) = send(&st, "GET", "/ngsi-ld/v1/entities?type=Car", &ANA, None).await?;
    assert_eq!((code, ids(&list)), (StatusCode::OK, car_ids(&[1, 2, 3])));
    Ok(())
}

#[tokio::test]
async fn a_policy_outside_its_validity_window_grants_nothing() -> Res {
    let past = with(
        grant("old", "group", "city", &["5.7.2"], cars(&[])),
        "validity",
        json!({"to": "2020-01-01T00:00:00Z"}),
    );
    let future = with(
        grant("new", "group", "city", &["5.7.2"], cars(&[])),
        "validity",
        json!({"from": "2999-01-01T00:00:00Z"}),
    );
    let (st, _) = broker(doc(&[past, future])).await?;
    fleet(&st).await?;
    assert_eq!(
        send(&st, "GET", QUERY, &ANA, None).await?.0,
        StatusCode::FORBIDDEN
    );
    Ok(())
}

#[tokio::test]
async fn asking_for_more_than_the_grant_covers_narrows_to_the_grant() -> Res {
    let (st, _) = broker(doc(&[grant(
        "city",
        "group",
        "city",
        &["5.7.2"],
        cars(&["speed"]),
    )]))
    .await?;
    fleet(&st).await?;
    let (code, list) = send(
        &st,
        "GET",
        &format!("{QUERY}&attrs=speed,colour,code"),
        &ANA,
        None,
    )
    .await?;
    assert_eq!(
        (code, ids(&list)),
        (StatusCode::OK, car_ids(&[1, 2, 3])),
        "{list}"
    );
    // a_whitelist_narrows_a_read_that_named_no_attribute
    let (_, list) = send(&st, "GET", QUERY, &ANA, None).await?;
    assert!(!list.to_string().contains("colour"), "{list}");
    // the restricted signal only on the caller's opt-in
    let opted = [ANA[0], ANA[1], ("x-jc-restricted", "true")];
    let resp = send(&st, "GET", QUERY, &opted, None).await?;
    assert_eq!(resp.0, StatusCode::OK);
    Ok(())
}

#[tokio::test]
async fn a_grant_without_an_attribute_list_is_not_narrowed_by_a_sibling_with_one() -> Res {
    let (st, _) = broker(doc(&[
        grant("speed", "group", "city", &["5.7.2"], cars(&["speed"])),
        grant("whole", "group", "city", &["5.7.2"], cars(&[])),
    ]))
    .await?;
    fleet(&st).await?;
    let (_, list) = send(&st, "GET", "/ngsi-ld/v1/entities?type=Car", &ANA, None).await?;
    assert!(list.to_string().contains("colour"), "{list}");
    Ok(())
}

#[tokio::test]
async fn several_grants_are_or_ed_with_each_other_and_and_ed_with_the_caller() -> Res {
    let slow = with(
        grant("slow", "group", "city", &["5.7.2"], cars(&[])),
        "q",
        json!("speed<20"),
    );
    let sasova = with(
        grant("sasova", "group", "city", &["5.7.2"], cars(&[])),
        "scopeQ",
        json!("/geo/SK/BB/Sasova"),
    );
    let (st, _) = broker(doc(&[slow, sasova])).await?;
    fleet(&st).await?;
    let (_, list) = send(&st, "GET", "/ngsi-ld/v1/entities?type=Car", &ANA, None).await?;
    assert_eq!(ids(&list), car_ids(&[1, 2]));
    let (_, list) = send(
        &st,
        "GET",
        "/ngsi-ld/v1/entities?type=Car&q=speed%3E50",
        &ANA,
        None,
    )
    .await?;
    assert_eq!(ids(&list), car_ids(&[2]));
    Ok(())
}

#[tokio::test]
async fn children_match_and_siblings_and_prefixes_do_not() -> Res {
    let bb = with(
        grant("bb", "group", "city", &["5.7.2"], cars(&[])),
        "scopeQ",
        json!("/geo/SK/BB"),
    );
    let (st, _) = broker(doc(&[bb])).await?;
    fleet(&st).await?;
    let (_, list) = send(&st, "GET", "/ngsi-ld/v1/entities?type=Car", &ANA, None).await?;
    assert_eq!(ids(&list), car_ids(&[1, 2]));
    // the_callers_scope_never_widens_the_granted_tree
    let (_, list) = send(
        &st,
        "GET",
        "/ngsi-ld/v1/entities?type=Car&scopeQ=/geo/SK/%23",
        &ANA,
        None,
    )
    .await?;
    assert_eq!(ids(&list), car_ids(&[1, 2]));
    Ok(())
}

#[tokio::test]
async fn an_exact_id_selector_compiles_into_a_pattern_that_matches_only_it() -> Res {
    let one = grant(
        "one",
        "group",
        "city",
        &["5.7.2", "5.6.2"],
        json!([{"entities": [{"type": "Car", "id": "urn:ngsi-ld:Car:1"}]}]),
    );
    let (st, _) = broker(doc(&[one])).await?;
    fleet(&st).await?;
    let (_, list) = send(&st, "GET", "/ngsi-ld/v1/entities?type=Car", &ANA, None).await?;
    assert_eq!(ids(&list), car_ids(&[1]));
    // an_identifier_outside_the_granted_type_or_pattern_is_refused_on_its_own
    let patch = json!({"speed": prop(json!(5))});
    let (code, _) = send(
        &st,
        "PATCH",
        "/ngsi-ld/v1/entities/urn:ngsi-ld:Car:2/attrs",
        &ANA,
        Some(&patch),
    )
    .await?;
    assert_eq!(code, StatusCode::FORBIDDEN);
    let (code, _) = send(
        &st,
        "PATCH",
        "/ngsi-ld/v1/entities/urn:ngsi-ld:Car:1/attrs",
        &ANA,
        Some(&patch),
    )
    .await?;
    assert_eq!(code, StatusCode::NO_CONTENT);
    Ok(())
}

#[tokio::test]
async fn bisecting_a_hidden_value_learns_nothing() -> Res {
    let (st, _) = broker(doc(&[grant(
        "city",
        "group",
        "city",
        &["5.7.2"],
        cars(&["speed"]),
    )]))
    .await?;
    fleet(&st).await?;
    for q in [
        "colour==%22red%22",
        "colour!=%22red%22",
        "!colour",
        "colour",
    ] {
        let (_, a) = send(
            &st,
            "GET",
            &format!("/ngsi-ld/v1/entities?type=Car&q={q}"),
            &ANA,
            None,
        )
        .await?;
        let reads_absent = q == "!colour";
        assert_eq!(
            ids(&a),
            if reads_absent {
                car_ids(&[1, 2, 3])
            } else {
                Vec::new()
            },
            "{q}"
        );
    }
    Ok(())
}

#[tokio::test]
async fn q_on_an_attribute_of_one_type_matches_that_type_only() -> Res {
    let (st, _) = broker(doc(&[grant(
        "two",
        "group",
        "city",
        &["5.7.2"],
        json!([{"entities": [{"type": "Car"}], "propertyNames": ["speed"]},
               {"entities": [{"type": "Secret"}], "propertyNames": ["code"]}]),
    )]))
    .await?;
    fleet(&st).await?;
    let (_, list) = send(&st, "GET", &format!("{QUERY}&q=code"), &ANA, None).await?;
    assert_eq!(ids(&list), ["urn:ngsi-ld:Secret:1"]);
    let (_, list) = send(&st, "GET", QUERY, &ANA, None).await?;
    let text = list.to_string();
    assert!(
        text.contains("code") && text.contains("speed") && !text.contains("colour"),
        "{text}"
    );
    Ok(())
}

/// Writes: the platform's write guard.
fn writer(information: Value) -> Value {
    grant(
        "w",
        "group",
        "city",
        &["5.6.1", "5.6.2", "5.6.3", "5.6.6"],
        information,
    )
}

#[tokio::test]
async fn a_write_inside_the_grant_passes() -> Res {
    let (st, _) = broker(doc(&[writer(json!([{"entities": [{"type": "Car"}]}]))])).await?;
    let (code, body) = send(
        &st,
        "POST",
        "/ngsi-ld/v1/entities",
        &ANA,
        Some(&car(7, 1, "/a")),
    )
    .await?;
    assert_eq!(code, StatusCode::CREATED, "{body}");
    Ok(())
}

#[tokio::test]
async fn a_write_touching_an_ungranted_attribute_or_type_is_refused_whole() -> Res {
    let (st, _) = broker(doc(&[writer(cars(&["speed"]))])).await?;
    fleet(&st).await?;
    // a_write_keeps_the_grants_attributes_whatever_the_query_names
    let colour = json!({"colour": prop(json!("blue"))});
    let (code, _) = send(
        &st,
        "PATCH",
        "/ngsi-ld/v1/entities/urn:ngsi-ld:Car:1/attrs",
        &ANA,
        Some(&colour),
    )
    .await?;
    assert_eq!(code, StatusCode::FORBIDDEN);
    let both = json!({"speed": prop(json!(1)), "colour": prop(json!("blue"))});
    let (code, _) = send(
        &st,
        "PATCH",
        "/ngsi-ld/v1/entities/urn:ngsi-ld:Car:1/attrs",
        &ANA,
        Some(&both),
    )
    .await?;
    assert_eq!(code, StatusCode::FORBIDDEN);
    let secret = json!({"id": "urn:ngsi-ld:Secret:2", "type": "Secret"});
    let (code, _) = send(&st, "POST", "/ngsi-ld/v1/entities", &ANA, Some(&secret)).await?;
    assert_eq!(code, StatusCode::FORBIDDEN);
    let (code, _) = send(
        &st,
        "DELETE",
        "/ngsi-ld/v1/entities/urn:ngsi-ld:Secret:1",
        &ANA,
        None,
    )
    .await?;
    assert_eq!(code, StatusCode::FORBIDDEN);
    let speed = json!({"speed": prop(json!(1))});
    let (code, _) = send(
        &st,
        "PATCH",
        "/ngsi-ld/v1/entities/urn:ngsi-ld:Car:1/attrs",
        &ANA,
        Some(&speed),
    )
    .await?;
    assert_eq!(code, StatusCode::NO_CONTENT);
    Ok(())
}

#[tokio::test]
async fn a_scope_outside_the_granted_subtree_is_refused() -> Res {
    let bb = with(
        writer(json!([{"entities": [{"type": "Car"}]}])),
        "scopeQ",
        json!("/geo/SK/BB"),
    );
    let (st, _) = broker(doc(&[bb])).await?;
    for (n, scope, want) in [
        (11, "/geo/SK/BBB", StatusCode::FORBIDDEN),
        (12, "/geo/SK", StatusCode::FORBIDDEN),
        (13, "/geo/SK/BB/Sasova", StatusCode::CREATED),
        (14, "/geo/SK/BB", StatusCode::CREATED),
    ] {
        let (code, body) = send(
            &st,
            "POST",
            "/ngsi-ld/v1/entities",
            &ANA,
            Some(&car(n, 1, scope)),
        )
        .await?;
        assert_eq!(code, want, "{scope}: {body}");
    }
    Ok(())
}

#[tokio::test]
async fn coordinates_outside_the_granted_area_are_refused() -> Res {
    let zone = with(
        writer(json!([{"entities": [{"type": "Car"}]}])),
        "geoQ",
        json!(AREA),
    );
    let (st, _) = broker(doc(&[zone])).await?;
    let at = |n: u32, x: f64| {
        json!({"id": format!("urn:ngsi-ld:Car:{n}"), "type": "Car",
               "location": {"type": "GeoProperty", "value": {"type": "Point", "coordinates": [x, 0.5]}}})
    };
    let (code, _) = send(
        &st,
        "POST",
        "/ngsi-ld/v1/entities",
        &ANA,
        Some(&at(21, 5.0)),
    )
    .await?;
    assert_eq!(code, StatusCode::FORBIDDEN);
    let (code, body) = send(
        &st,
        "POST",
        "/ngsi-ld/v1/entities",
        &ANA,
        Some(&at(22, 0.5)),
    )
    .await?;
    assert_eq!(code, StatusCode::CREATED, "{body}");
    // a write by id under an area is judged on the stored Entity, and the
    // post-image's own location must stay inside
    let away = json!({"location": {"type": "GeoProperty", "value": {"type": "Point", "coordinates": [5.0, 0.5]}}});
    let (code, _) = send(
        &st,
        "PATCH",
        "/ngsi-ld/v1/entities/urn:ngsi-ld:Car:22/attrs",
        &ANA,
        Some(&away),
    )
    .await?;
    assert_eq!(code, StatusCode::FORBIDDEN);
    Ok(())
}

#[tokio::test]
async fn an_entity_carrying_its_own_access_control_is_refused() -> Res {
    let (st, _) = broker(doc(&[writer(json!([{"entities": [{"type": "Car"}]}]))])).await?;
    let smuggled = json!({"id": "urn:ngsi-ld:Car:31", "type": "Car", "owner": prop(json!("me"))});
    let (code, body) = send(&st, "POST", "/ngsi-ld/v1/entities", &ANA, Some(&smuggled)).await?;
    assert_eq!(code, StatusCode::FORBIDDEN, "{body}");
    Ok(())
}

/// G12: a write by id under a condition on stored values is judged on the
/// stored Entity, inside the store's own read-modify-write.
#[tokio::test]
async fn a_write_by_id_under_a_condition_is_judged_on_the_stored_entity() -> Res {
    let slow = with(
        writer(json!([{"entities": [{"type": "Car"}]}])),
        "q",
        json!("speed<50"),
    );
    let (st, _) = broker(doc(&[slow])).await?;
    fleet(&st).await?;
    let patch = json!({"colour": prop(json!("blue"))});
    let (code, _) = send(
        &st,
        "PATCH",
        "/ngsi-ld/v1/entities/urn:ngsi-ld:Car:1/attrs",
        &ANA,
        Some(&patch),
    )
    .await?;
    assert_eq!(code, StatusCode::NO_CONTENT);
    let (code, _) = send(
        &st,
        "PATCH",
        "/ngsi-ld/v1/entities/urn:ngsi-ld:Car:2/attrs",
        &ANA,
        Some(&patch),
    )
    .await?;
    assert_eq!(code, StatusCode::NOT_FOUND);
    // a create is judged on its body
    let (code, _) = send(
        &st,
        "POST",
        "/ngsi-ld/v1/entities",
        &ANA,
        Some(&car(41, 99, "/a")),
    )
    .await?;
    assert_eq!(code, StatusCode::FORBIDDEN);
    let (code, _) = send(
        &st,
        "POST",
        "/ngsi-ld/v1/entities",
        &ANA,
        Some(&car(42, 9, "/a")),
    )
    .await?;
    assert_eq!(code, StatusCode::CREATED);
    Ok(())
}

/// M22: a call carrying no subject reaches the Context Source operations
/// and nothing else.
#[tokio::test]
async fn a_call_without_a_subject_reaches_registrations_only() -> Res {
    let (st, _) = broker(doc(&[])).await?;
    let csr = json!({"id": "urn:ngsi-ld:ContextSourceRegistration:1", "type": "ContextSourceRegistration",
                     "information": [{"entities": [{"type": "Car"}]}],
                     "endpoint": "http://127.0.0.1:9/ngsi-ld"});
    let (code, body) = send(
        &st,
        "POST",
        "/ngsi-ld/v1/csourceRegistrations",
        &[],
        Some(&csr),
    )
    .await?;
    assert_eq!(code, StatusCode::CREATED, "{body}");
    let (code, _) = send(
        &st,
        "GET",
        "/ngsi-ld/v1/csourceRegistrations?type=Car",
        &[],
        None,
    )
    .await?;
    assert_eq!(code, StatusCode::OK);
    let (code, _) = send(&st, "GET", QUERY, &[], None).await?;
    assert_eq!(code, StatusCode::FORBIDDEN);
    Ok(())
}

#[tokio::test]
async fn a_document_the_engine_refuses_keeps_the_previous_one() -> Res {
    let (st, engine) = broker(doc(&[grant(
        "city",
        "group",
        "city",
        &["5.7.2"],
        cars(&[]),
    )]))
    .await?;
    fleet(&st).await?;
    let refused = engine.load("default", &json!({"policies": [{"id": "x"}]}));
    assert!(refused.is_err(), "{refused:?}");
    let (code, list) = send(&st, "GET", "/ngsi-ld/v1/entities?type=Car", &ANA, None).await?;
    assert_eq!((code, ids(&list)), (StatusCode::OK, car_ids(&[1, 2, 3])));
    assert!(engine.loads_documents() && engine.name() == joinedcontext_policy::POLICY_NAME);
    Ok(())
}

#[tokio::test]
async fn a_tenant_wide_operation_needs_a_policy_that_narrows_nothing() -> Res {
    let narrowed = grant("purge", "group", "city", &["5.6.21"], cars(&[]));
    let (st, engine) = broker(doc(&[narrowed])).await?;
    let subject = antares_api::policy::Subject {
        tenant: antares_model::TenantId::default(),
        headers: vec![
            ("x-jc-groups".into(), "city".into()),
            ("x-jc-subject".into(), "a".into()),
        ],
    };
    let op = antares_api::policy::Operation::new("5.6.21");
    assert!(matches!(
        engine.decide(&subject, &op).await,
        antares_api::policy::Decision::Deny(_)
    ));
    engine.load(
        "default",
        &doc(&[grant("purge", "group", "city", &["5.6.21"], json!([]))]),
    )?;
    assert_eq!(
        engine.decide(&subject, &op).await,
        antares_api::policy::Decision::Allow
    );
    drop(st);
    Ok(())
}

/// K25, notification egress: the notification is decided again for the
/// Subscription's subject, so a revoked grant stops the next delivery.
#[tokio::test]
async fn a_revoked_grant_stops_the_subscription_it_allowed() -> Res {
    use antares_api::policy::{NotifyDecision, Subject};
    let (_, engine) = broker(doc(&[grant(
        "city",
        "group",
        "city",
        &["5.7.2"],
        cars(&["speed"]),
    )]))
    .await?;
    let subject = Subject {
        tenant: antares_model::TenantId::default(),
        headers: vec![
            ("x-jc-subject".into(), "ana".into()),
            ("x-jc-groups".into(), "city".into()),
        ],
    };
    let mut n = json!({"data": [car(1, 10, "/a")]});
    assert!(matches!(
        engine.pre_notify(&subject, &json!({}), &mut n),
        NotifyDecision::Filter(_)
    ));
    engine.load("default", &doc(&[]))?;
    assert_eq!(
        engine.pre_notify(&subject, &json!({}), &mut n),
        NotifyDecision::Drop
    );
    engine.load(
        "default",
        &doc(&[grant("all", "group", "city", &["5.7.2"], json!([]))]),
    )?;
    assert_eq!(
        engine.pre_notify(&subject, &json!({}), &mut n),
        NotifyDecision::Deliver
    );
    Ok(())
}

/// A policy granting every type under a condition writes a rule per type
/// the read names, and refuses a read that names none.
#[tokio::test]
async fn a_conditional_grant_over_every_type_needs_the_types_named() -> Res {
    let slow = with(
        grant("slow", "group", "city", &["5.7.2", "5.7.1"], json!([])),
        "q",
        json!("speed<50"),
    );
    let (st, _) = broker(doc(&[slow])).await?;
    fleet(&st).await?;
    let (_, list) = send(&st, "GET", "/ngsi-ld/v1/entities?type=Car", &ANA, None).await?;
    assert_eq!(ids(&list), car_ids(&[1, 3]));
    let (code, _) = send(&st, "GET", "/ngsi-ld/v1/entities?q=speed%3E0", &ANA, None).await?;
    assert_eq!(code, StatusCode::FORBIDDEN);
    Ok(())
}

/// T-3770: the gateway percent-encodes each name, so a Slovak group reaches the engine whole.
#[tokio::test]
async fn a_policy_for_a_group_with_a_diacritic_matches_its_encoded_member() -> Res {
    let (st, _) = broker(doc(&[grant(
        "sprava",
        "group",
        "doprava/správa",
        &["5.7.2"],
        cars(&["speed"]),
    )]))
    .await?;
    fleet(&st).await?;
    let jozo = [
        ("x-jc-subject", "jo%C5%BEo"),
        ("x-jc-groups", "city,doprava%2Fspr%C3%A1va"),
    ];
    let (code, list) = send(&st, "GET", QUERY, &jozo, None).await?;
    assert_eq!(code, StatusCode::OK, "{list}");
    assert_eq!(ids(&list), car_ids(&[1, 2, 3]));
    // a broken escape is that member dropped, never a wider caller
    let broken = [
        ("x-jc-subject", "jo%C5"),
        ("x-jc-groups", "doprava%2Fspr%C3"),
    ];
    assert_eq!(
        send(&st, "GET", QUERY, &broken, None).await?.0,
        StatusCode::FORBIDDEN
    );
    Ok(())
}

/// `doc` with the views member (README section 2, T-3702).
fn bound(policies: &[Value], views: Value) -> Value {
    with(doc(policies), "views", views)
}

async fn through(
    view: &str,
    st: &AppState,
    method: &str,
    uri: &str,
    who: &[(&str, &str)],
    body: Option<&Value>,
) -> Result<(StatusCode, Value), String> {
    antares_api::policy::through_view(view, send(st, method, uri, who, body)).await
}

fn secrets() -> Value {
    json!([{"entities": [{"type": "Secret"}]}])
}

/// T-3702: a subject holding Policy B reads nothing of B through a view
/// bound to Policy A; the plain root still decides with both.
#[tokio::test]
async fn a_view_bound_to_one_policy_is_not_widened_by_another_the_caller_holds() -> Res {
    let a = grant("a", "group", "city", &["5.7.1", "5.7.2"], cars(&[]));
    let b = grant("b", "group", "city", &["5.7.1", "5.7.2"], secrets());
    let (st, _) = broker(bound(&[a, b], json!({"cars": ["a"]}))).await?;
    fleet(&st).await?;

    let (code, list) = through("cars", &st, "GET", QUERY, &ANA, None).await?;
    assert_eq!((code, ids(&list)), (StatusCode::OK, car_ids(&[1, 2, 3])));
    let (code, body) = through(
        "cars",
        &st,
        "GET",
        "/ngsi-ld/v1/entities/urn:ngsi-ld:Secret:1",
        &ANA,
        None,
    )
    .await?;
    assert_eq!(code, StatusCode::NOT_FOUND, "{body}");

    let (code, list) = send(&st, "GET", QUERY, &ANA, None).await?;
    let mut all = car_ids(&[1, 2, 3]);
    all.push("urn:ngsi-ld:Secret:1".into());
    assert_eq!((code, ids(&list)), (StatusCode::OK, all));
    Ok(())
}

/// T-3702: an App's view is decided by the App's own grant and the space's
/// prohibition alone: another grant of the same caller adds nothing there,
/// and the prohibition still holds.
#[tokio::test]
async fn an_app_view_is_decided_by_its_own_grant_and_the_prohibition() -> Res {
    const APP: [(&str, &str); 2] = [("x-jc-client", "app-x"), ("x-jc-groups", "city")];
    let own = grant(
        "own",
        "serviceAccount",
        "app-x",
        &["5.7.1", "5.7.2"],
        json!([
            {"entities": [{"type": "Car"}, {"type": "Secret"}]}
        ]),
    );
    let no_secret = with(
        grant("no-secret", "group", "city", &["5.7.2"], secrets()),
        "effect",
        json!("prohibit"),
    );
    let writes = grant("writes", "group", "city", &["5.6.1"], cars(&[]));
    let (st, _) = broker(bound(
        &[own, no_secret, writes],
        json!({"app-x": ["own", "no-secret"]}),
    ))
    .await?;
    fleet(&st).await?;

    let (code, list) = through(
        "app-x",
        &st,
        "GET",
        "/ngsi-ld/v1/entities?type=Car",
        &APP,
        None,
    )
    .await?;
    assert_eq!((code, ids(&list)), (StatusCode::OK, car_ids(&[1, 2, 3])));
    let (code, body) = through("app-x", &st, "GET", QUERY, &APP, None).await?;
    assert_eq!(code, StatusCode::FORBIDDEN, "the prohibition holds: {body}");
    let new = car(9, 1, "/geo/SK/BB");
    let (code, body) = through(
        "app-x",
        &st,
        "POST",
        "/ngsi-ld/v1/entities",
        &APP,
        Some(&new),
    )
    .await?;
    assert_eq!(
        code,
        StatusCode::FORBIDDEN,
        "the other grant adds nothing: {body}"
    );
    let (code, body) = send(&st, "POST", "/ngsi-ld/v1/entities", &APP, Some(&new)).await?;
    assert_eq!(
        code,
        StatusCode::CREATED,
        "the plain root has every policy: {body}"
    );
    Ok(())
}

/// T-3702: a view the document does not list is decided by every policy.
#[tokio::test]
async fn a_view_not_listed_is_decided_by_every_policy() -> Res {
    let a = grant("a", "group", "city", &["5.7.2"], cars(&[]));
    let b = grant("b", "group", "city", &["5.7.2"], secrets());
    let (st, _) = broker(bound(&[a, b], json!({"cars": ["a"]}))).await?;
    fleet(&st).await?;
    let (code, list) = through("elsewhere", &st, "GET", QUERY, &ANA, None).await?;
    assert_eq!(code, StatusCode::OK, "{list}");
    assert_eq!(ids(&list).len(), 4, "{list}");
    Ok(())
}

/// T-3702: a view naming a policy the document lacks, or one twice, is
/// refused and the previous document keeps deciding; a view may list none.
#[tokio::test]
async fn a_view_list_that_does_not_match_the_document_is_refused() -> Res {
    let a = grant("a", "group", "city", &["5.7.2"], cars(&[]));
    let engine = JoinedcontextPolicy::new();
    for views in [
        json!({"cars": ["a", "a"]}),
        json!({"cars": ["nope"]}),
        json!({"cars": "a"}),
    ] {
        let refused = engine.load("default", &bound(std::slice::from_ref(&a), views.clone()));
        assert!(refused.is_err(), "{views}");
    }
    engine.load(
        "default",
        &bound(std::slice::from_ref(&a), json!({"closed": []})),
    )?;
    let (st, _) = broker(bound(&[a], json!({"closed": []}))).await?;
    fleet(&st).await?;
    let (code, body) = through("closed", &st, "GET", QUERY, &ANA, None).await?;
    assert_eq!(
        code,
        StatusCode::FORBIDDEN,
        "a view listing nothing grants nothing: {body}"
    );
    Ok(())
}

/// T-3796 (MP-02): through a view, a filter on an attribute the grant does
/// not show answers nothing and says which type it left out, as the gateway
/// did; a filter on what it shows says nothing.
#[tokio::test]
async fn a_view_names_the_type_its_filter_left_out() -> Res {
    let a = grant("a", "group", "city", &["5.7.1", "5.7.2"], cars(&["speed"]));
    let (st, _) = broker(bound(&[a], json!({"cars": ["a"]}))).await?;
    fleet(&st).await?;
    let read = |uri: &'static str| {
        let st = st.clone();
        antares_api::policy::through_view("cars", async move {
            exchange(&st, "GET", uri, &ANA, None).await
        })
    };

    let (code, headers, list) = read("/ngsi-ld/v1/entities?type=Car&q=colour==%22red%22").await?;
    assert_eq!((code, list), (StatusCode::OK, json!([])));
    let warned: Vec<&str> = headers
        .get_all("NGSILD-Warning")
        .iter()
        .filter_map(|v| v.to_str().ok())
        .collect();
    assert!(
        warned.iter().any(|w| w.starts_with("199 ")
            && w.contains("Car was not queried: this request selects or orders on an attribute")),
        "{warned:?}"
    );

    let (code, headers, list) = read("/ngsi-ld/v1/entities?type=Car&q=speed%3E20").await?;
    assert_eq!((code, ids(&list)), (StatusCode::OK, car_ids(&[2, 3])));
    assert!(headers.get("NGSILD-Warning").is_none(), "{headers:?}");
    Ok(())
}
