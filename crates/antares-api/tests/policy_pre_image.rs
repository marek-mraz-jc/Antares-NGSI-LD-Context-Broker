// SPDX-License-Identifier: EUPL-1.2
//! What a write by id hands the policy engine about the Entity it lands on
//! (ADR-0023 rule 7, operation matrix item 2, T-3483).
//!
//! An update, a delete or a replace names an id and, at most, the members
//! it changes; ADR-N-041 forbids reading a type or a right off the URN. So
//! the gate hands the engine the stored Entity's image (types, scope,
//! Attribute names, expanded) as `Operation::stored`, and an engine judges
//! a write by what the Entity is, not by what the caller says it is.
#![cfg(feature = "test-kit")]
#![allow(clippy::unwrap_used)] // an unwrap here is the assertion

use antares_api::policy::{
    Decision, DecisionFuture, NotifyDecision, Operation, PolicyEngine, StoredImage, Subject,
};
use antares_api::AppState;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use serde_json::{json, Value};
use std::sync::{Arc, Mutex};
use tower::ServiceExt;

const DC: &str = "https://uri.etsi.org/ngsi-ld/default-context/";
const VEHICLE: &str = "urn:ngsi-ld:Vehicle:v";
const SECRET: &str = "urn:ngsi-ld:Secret:s";
const ABSENT: &str = "urn:ngsi-ld:Vehicle:absent";

type Log = Arc<Mutex<Vec<(String, Vec<String>, Option<StoredImage>)>>>;

/// Records the clause, the ids and the stored image of every decision;
/// refuses a write whose STORED Entity is a `Secret`, whatever the request
/// names, so the refusal can only come from the pre-image.
struct Recording(Log);

impl PolicyEngine for Recording {
    fn name(&self) -> &str {
        "recording"
    }

    fn decide<'a>(&'a self, _s: &'a Subject, op: &'a Operation<'a>) -> DecisionFuture<'a> {
        if let Ok(mut v) = self.0.lock() {
            v.push((
                op.clause.to_owned(),
                op.ids.iter().map(|i| (*i).to_owned()).collect(),
                op.stored.cloned(),
            ));
        }
        let secret = format!("{DC}Secret");
        Box::pin(std::future::ready(
            match op.stored.is_some_and(|s| s.types.contains(&secret)) {
                true => Decision::Deny("the stored Entity is a Secret".into()),
                false => Decision::Allow,
            },
        ))
    }

    fn pre_notify(&self, _s: &Subject, _sub: &Value, _n: &mut Value) -> NotifyDecision {
        NotifyDecision::Deliver
    }
}

async fn state() -> (AppState, Log) {
    let seen = Arc::new(Mutex::new(Vec::new()));
    let mut st = AppState::new("me".into()).with_policy(Arc::new(Recording(Arc::clone(&seen))));
    antares_api::wire(&mut st).await;
    (st, seen)
}

async fn send(st: &AppState, method: &str, uri: &str, body: Option<Value>) -> StatusCode {
    let mut b = Request::builder().method(method).uri(uri);
    let req = match body {
        Some(v) => {
            let payload = v.to_string();
            b = b
                .header("Content-Type", "application/json")
                .header("Content-Length", payload.len());
            b.body(Body::from(payload)).expect("request")
        }
        None => b.body(Body::empty()).expect("request"),
    };
    antares_api::router(st.clone())
        .oneshot(req)
        .await
        .expect("response")
        .status()
}

/// A scoped Vehicle every write below lands on, and the Secret the engine
/// guards.
async fn seed(st: &AppState) {
    for doc in [
        json!({"id": VEHICLE, "type": "Vehicle", "scope": "/hel/a",
               "speed": {"type": "Property", "value": 1},
               "colour": {"type": "Property", "value": "red"}}),
        json!({"id": SECRET, "type": "Secret",
               "code": {"type": "Property", "value": 7}}),
    ] {
        assert_eq!(
            send(st, "POST", "/ngsi-ld/v1/entities", Some(doc)).await,
            StatusCode::CREATED
        );
    }
}

fn vehicle_image() -> StoredImage {
    StoredImage {
        types: vec![format!("{DC}Vehicle")],
        scope: vec!["/hel/a".into()],
        attribute_names: vec![format!("{DC}colour"), format!("{DC}speed")],
    }
}

/// The image the engine was shown for the last decision on `clause`, with
/// its Attribute names sorted (a document's member order is the store's).
fn image_for(seen: &Log, clause: &str) -> Option<StoredImage> {
    let log = seen.lock().unwrap();
    let (_, _, stored) = log
        .iter()
        .rev()
        .find(|(c, _, _)| c == clause)
        .unwrap_or_else(|| panic!("the engine was not asked about {clause}: {log:?}"));
    stored.clone().map(|mut s| {
        s.attribute_names.sort();
        s
    })
}

/// Every write by id on an existing Entity shows the engine its stored
/// types, scope and Attribute names, whatever the request body carries.
#[tokio::test(flavor = "multi_thread")]
async fn every_write_by_id_shows_the_engine_the_stored_entity() {
    let attrs = format!("/ngsi-ld/v1/entities/{VEHICLE}/attrs");
    let property = |v: Value| json!({"type": "Property", "value": v});
    for (clause, method, uri, body) in [
        (
            "5.6.2",
            "PATCH",
            attrs.clone(),
            Some(json!({"speed": property(json!(2))})),
        ),
        (
            "5.6.3",
            "POST",
            attrs.clone(),
            Some(json!({"weight": property(json!(3))})),
        ),
        (
            "5.6.4",
            "PATCH",
            format!("{attrs}/speed"),
            Some(property(json!(4))),
        ),
        (
            "5.6.19",
            "PUT",
            format!("{attrs}/speed"),
            Some(json!({"speed": property(json!(5))})),
        ),
        (
            "5.6.17",
            "PATCH",
            format!("/ngsi-ld/v1/entities/{VEHICLE}"),
            Some(json!({"speed": property(json!(6))})),
        ),
        (
            "5.6.18",
            "PUT",
            format!("/ngsi-ld/v1/entities/{VEHICLE}"),
            Some(json!({"id": VEHICLE, "type": "Vehicle", "speed": property(json!(7))})),
        ),
        ("5.6.5", "DELETE", format!("{attrs}/colour"), None),
        (
            "5.6.6",
            "DELETE",
            format!("/ngsi-ld/v1/entities/{VEHICLE}"),
            None,
        ),
    ] {
        // a fresh Entity each time: a replace or a delete changes it
        let (st, seen) = state().await;
        seed(&st).await;
        let code = send(&st, method, &uri, body).await;
        assert!(code.is_success(), "{clause} {method} {uri}: {code}");
        assert_eq!(
            image_for(&seen, clause),
            Some(vehicle_image()),
            "{clause} did not show the engine the stored Entity"
        );
    }
}

/// An engine that refuses by the stored type refuses the write even when
/// the request names another type, and the store is unchanged.
#[tokio::test(flavor = "multi_thread")]
async fn an_engine_refusing_on_the_stored_type_leaves_the_store_unchanged() {
    let (st, _) = state().await;
    seed(&st).await;
    let entity = format!("/ngsi-ld/v1/entities/{SECRET}");
    assert_eq!(
        send(&st, "DELETE", &entity, None).await,
        StatusCode::FORBIDDEN
    );
    assert_eq!(
        send(
            &st,
            "PUT",
            &entity,
            Some(json!({"id": SECRET, "type": "Vehicle",
                        "code": {"type": "Property", "value": 0}})),
        )
        .await,
        StatusCode::FORBIDDEN,
        "a body naming another type does not move the Entity out of the engine's rule"
    );
    assert_eq!(
        send(&st, "DELETE", &format!("{entity}/attrs/code"), None).await,
        StatusCode::FORBIDDEN
    );
    assert_eq!(
        send(&st, "GET", &entity, None).await,
        StatusCode::OK,
        "the Secret was removed"
    );
}

/// A write to an id the tenant does not hold hands the engine no image and
/// answers as it always did.
#[tokio::test(flavor = "multi_thread")]
async fn a_write_to_an_absent_id_hands_no_image_and_answers_as_before() {
    let (st, seen) = state().await;
    seed(&st).await;
    assert_eq!(
        send(
            &st,
            "PATCH",
            &format!("/ngsi-ld/v1/entities/{ABSENT}/attrs"),
            Some(json!({"speed": {"type": "Property", "value": 1}})),
        )
        .await,
        StatusCode::NOT_FOUND
    );
    assert_eq!(image_for(&seen, "5.6.2"), None);
    assert_eq!(
        send(
            &st,
            "DELETE",
            &format!("/ngsi-ld/v1/entities/{ABSENT}"),
            None
        )
        .await,
        StatusCode::NOT_FOUND
    );
    assert_eq!(image_for(&seen, "5.6.6"), None);
}

/// Each member of a batch carries its own image: the stored one where the
/// id exists, none where it does not.
#[tokio::test(flavor = "multi_thread")]
async fn each_batch_member_carries_its_own_image() {
    for (clause, path, body) in [
        (
            "5.6.8",
            "upsert",
            json!([
                {"id": VEHICLE, "type": "Vehicle", "speed": {"type": "Property", "value": 9}},
                {"id": ABSENT, "type": "Vehicle", "speed": {"type": "Property", "value": 9}},
            ]),
        ),
        ("5.6.10", "delete", json!([VEHICLE, ABSENT])),
    ] {
        let (st, seen) = state().await;
        seed(&st).await;
        let code = send(
            &st,
            "POST",
            &format!("/ngsi-ld/v1/entityOperations/{path}"),
            Some(body),
        )
        .await;
        assert!(
            code.is_success() || code == StatusCode::MULTI_STATUS,
            "{path}: {code}"
        );
        let log = seen.lock().unwrap();
        let images: Vec<(String, Option<Vec<String>>)> = log
            .iter()
            .filter(|(c, _, _)| c == clause)
            .map(|(_, ids, stored)| {
                (
                    ids.first().cloned().unwrap_or_default(),
                    stored.as_ref().map(|s| s.types.clone()),
                )
            })
            .collect();
        assert_eq!(
            images,
            vec![
                (VEHICLE.to_owned(), Some(vec![format!("{DC}Vehicle")])),
                (ABSENT.to_owned(), None),
            ],
            "{path}"
        );
    }
}

/// Without an engine the writes answer exactly as before: nothing reads
/// a pre-image, nothing refuses.
#[tokio::test(flavor = "multi_thread")]
async fn allow_all_answers_unchanged() {
    let mut st = AppState::new("me".into());
    antares_api::wire(&mut st).await;
    seed(&st).await;
    assert_eq!(
        send(
            &st,
            "PATCH",
            &format!("/ngsi-ld/v1/entities/{SECRET}/attrs"),
            Some(json!({"code": {"type": "Property", "value": 8}})),
        )
        .await,
        StatusCode::NO_CONTENT
    );
    assert_eq!(
        send(
            &st,
            "DELETE",
            &format!("/ngsi-ld/v1/entities/{SECRET}"),
            None
        )
        .await,
        StatusCode::NO_CONTENT
    );
}
