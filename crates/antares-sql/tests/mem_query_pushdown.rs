// SPDX-License-Identifier: EUPL-1.2
//! T-3556: the memory and file stores filter a query on the stored documents
//! and copy only what the answer holds. ids, types and attrs are exact there,
//! so with nothing else asked the outcome is `decided` and a page is cut in
//! id order, the store's default, with its total; a `q`, `scopeQ` or geo
//! query leaves the caller the arbiter over the type-narrowed rows.

use antares_model::TenantId;
use antares_sql::store::any::AnyStore;
use antares_sql::store::filter::{EntityFilter, Page};
use antares_sql::store::{Kind, Store};
use serde_json::{json, Value};

const T: &str = "https://example.org/T";
const U: &str = "https://example.org/U";
const SPEED: &str = "https://example.org/speed";

async fn seeded() -> (AnyStore, TenantId) {
    let s = AnyStore::Mem(Store::default());
    let t = TenantId::new("mempush").expect("tenant");
    let docs = [
        json!({"id": "urn:ngsi-ld:T:1", "type": [T], SPEED: [{"type": "Property", "value": 1}]}),
        json!({"id": "urn:ngsi-ld:T:2", "type": [T]}),
        json!({"id": "urn:ngsi-ld:T:3", "type": [T], SPEED: [{"type": "Property", "value": 3}]}),
        json!({"id": "urn:ngsi-ld:T:4", "type": [T], "expiresAt": "2000-01-01T00:00:00Z"}),
        json!({"id": "urn:ngsi-ld:U:1", "type": [U], SPEED: [{"type": "Property", "value": 9}]}),
    ];
    for d in docs {
        let id = d["id"].as_str().expect("id").to_owned();
        assert!(s.create(&t, Kind::Entity, &id, d).await.expect("create"));
    }
    (s, t)
}

fn ids(rows: &[Value]) -> Vec<&str> {
    rows.iter().filter_map(|r| r["id"].as_str()).collect()
}

#[tokio::test]
async fn a_type_query_is_decided_and_paged_in_id_order() {
    let (s, t) = seeded().await;
    let types = vec![vec![T.to_owned()]];
    let page = |offset, count| {
        Some(Page {
            offset,
            limit: 1,
            count,
        })
    };
    let out = s
        .query_entities(
            &t,
            &EntityFilter {
                types: Some(&types),
                page: page(1, true),
                ..Default::default()
            },
        )
        .await
        .expect("query");
    assert!(out.decided && out.paged);
    assert_eq!(
        ids(&out.rows),
        ["urn:ngsi-ld:T:2"],
        "the second of T:1..T:3"
    );
    assert_eq!(out.total, Some(3), "the expired T:4 is not a match");

    // uncounted: the total says only whether a next page exists
    let out = s
        .query_entities(
            &t,
            &EntityFilter {
                types: Some(&types),
                page: page(0, false),
                ..Default::default()
            },
        )
        .await
        .expect("query");
    assert_eq!(ids(&out.rows), ["urn:ngsi-ld:T:1"]);
    assert_eq!(out.total, Some(2), "offset 0 + 1 row + a next page");

    // off the end: no rows, and the count still holds
    let out = s
        .query_entities(
            &t,
            &EntityFilter {
                types: Some(&types),
                page: page(9, true),
                ..Default::default()
            },
        )
        .await
        .expect("query");
    assert!(out.rows.is_empty());
    assert_eq!(out.total, Some(3));
}

#[tokio::test]
async fn ids_attrs_and_projection_apply_in_the_store() {
    let (s, t) = seeded().await;
    let want = [
        "urn:ngsi-ld:T:3",
        "urn:ngsi-ld:U:1",
        "urn:ngsi-ld:T:2",
        "urn:ngsi-ld:none",
    ];
    let attrs = vec![SPEED.to_owned()];
    let keep: Vec<String> = Vec::new();
    let out = s
        .query_entities(
            &t,
            &EntityFilter {
                ids: Some(&want),
                attrs: Some(&attrs),
                keep_attrs: Some(&keep),
                ..Default::default()
            },
        )
        .await
        .expect("query");
    assert!(out.decided && !out.paged);
    assert_eq!(
        ids(&out.rows),
        ["urn:ngsi-ld:T:3", "urn:ngsi-ld:U:1"],
        "id order"
    );
    assert!(
        out.rows.iter().all(|r| r.get(SPEED).is_none()),
        "kept no attribute"
    );
    assert!(
        out.rows.iter().all(|r| r.get("type").is_some()),
        "the frame stays"
    );
}

#[tokio::test]
async fn a_q_leaves_the_caller_the_arbiter_over_the_narrowed_rows() {
    let (s, t) = seeded().await;
    let types = vec![vec![T.to_owned()]];
    let q = antares_ql::parse_q("speed>2").expect("q");
    let out = s
        .query_entities(
            &t,
            &EntityFilter {
                types: Some(&types),
                q: Some(&q),
                page: Some(Page {
                    offset: 0,
                    limit: 1,
                    count: true,
                }),
                ..Default::default()
            },
        )
        .await
        .expect("query");
    assert!(!out.decided && !out.paged && out.total.is_none());
    assert_eq!(
        ids(&out.rows),
        ["urn:ngsi-ld:T:1", "urn:ngsi-ld:T:2", "urn:ngsi-ld:T:3"],
        "type-narrowed, unpaged, the expired one left out"
    );
}
