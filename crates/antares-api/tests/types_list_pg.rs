// SPDX-License-Identifier: EUPL-1.2
//! `GET /types` through a view on Postgres answers in under 200 ms over
//! 100 000 Entities (T-4683). Probing a type a rule does not name as the
//! two-type group `[name, ruleType]` matched nothing and walked the Tenant's
//! whole primary key for its `ORDER BY id LIMIT`: 17 s for MCP `list_types`
//! on dev, and the assistant routed without the source. Skips loudly without
//! `ANTARES_TEST_DATABASE_URL` (container recipe in `antares-sql/tests/pg.rs`).
#![cfg(feature = "postgres")]

use antares_api::policy::{
    Decision, DecisionFuture, Filter, NotifyDecision, Operation, PolicyEngine, Rule, Subject,
};
use antares_api::AppState;
use antares_model::TenantId;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use serde_json::Value;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tower::ServiceExt;

const DEFAULT: &str = "https://uri.etsi.org/ngsi-ld/default-context/";

fn iri(term: &str) -> String {
    format!("{DEFAULT}{term}")
}

struct Narrowing(Filter);

impl PolicyEngine for Narrowing {
    fn name(&self) -> &str {
        "types-list-pg"
    }

    fn decide<'a>(&'a self, _s: &'a Subject, op: &'a Operation<'a>) -> DecisionFuture<'a> {
        let answer = if antares_api::policy::FILTERABLE.contains(&op.clause) {
            Decision::Filter(self.0.clone())
        } else {
            Decision::Allow
        };
        Box::pin(std::future::ready(answer))
    }

    fn pre_notify(&self, _s: &Subject, _sub: &Value, _n: &mut Value) -> NotifyDecision {
        NotifyDecision::Filter(self.0.clone())
    }
}

/// Shows Cars and Buses, the Cars only in zone `a`.
fn view() -> Filter {
    let rule = |t: &str, q: Option<&str>| Rule {
        types: vec![iri(t)],
        q: q.map(|q| Box::new(antares_ql::parse_q(q).expect("q"))),
        attributes: vec![iri("zone")],
        ..Rule::default()
    };
    Filter {
        rules: vec![rule("Car", Some("zone==\"a\"")), rule("Bus", None)],
        ..Filter::default()
    }
}

async fn types(st: &AppState, tenant: &str) -> (StatusCode, Value, Duration) {
    types_at(st, tenant, "/ngsi-ld/v1/types").await
}

async fn types_at(st: &AppState, tenant: &str, uri: &str) -> (StatusCode, Value, Duration) {
    let req = Request::builder()
        .uri(uri)
        .header("NGSILD-Tenant", tenant)
        .body(Body::empty())
        .expect("request");
    let started = Instant::now();
    let res = antares_api::router(st.clone())
        .oneshot(req)
        .await
        .expect("response");
    let status = res.status();
    let bytes = axum::body::to_bytes(res.into_body(), usize::MAX)
        .await
        .expect("body");
    (
        status,
        serde_json::from_slice(&bytes).unwrap_or(Value::Null),
        started.elapsed(),
    )
}

/// 20 000 Entities of each of five types in zone `b`, one Car in zone `a`
/// that is a Tree too: through the view, Bus, Car and Tree, in under 200 ms.
#[tokio::test(flavor = "multi_thread")]
async fn a_views_type_list_over_100k_entities_answers_in_under_200_ms() {
    let Ok(url) = std::env::var("ANTARES_TEST_DATABASE_URL") else {
        eprintln!("SKIP: ANTARES_TEST_DATABASE_URL not set");
        return;
    };
    let pool = antares_sql::store::pg::connect(&url, 5)
        .await
        .expect("connect");
    let tenant = format!("tl{}", &uuid::Uuid::new_v4().simple().to_string()[..12]);
    let tid = TenantId::new(&tenant).expect("tenant");
    antares_sql::store::pg::ensure_tenant(&pool, &tid)
        .await
        .expect("tenant row");
    // RLS reads the tenant from the session; the bulk insert sets it too.
    let mut tx = pool.begin().await.expect("tx");
    antares_sql::sqlx::query("SELECT set_config('antares.tenant', $1, true)")
        .bind(&tenant)
        .execute(&mut *tx)
        .await
        .expect("tenant setting");
    antares_sql::sqlx::query(
        "INSERT INTO entities (tenant_id, id, entity, types, created_at, modified_at)
         SELECT $1, 'urn:ngsi-ld:' || t || ':' || lpad(n::text, 6, '0'),
                jsonb_build_object('id', 'urn:ngsi-ld:' || t || ':' || lpad(n::text, 6, '0'),
                                   'type', jsonb_build_array($2 || t),
                                   $2 || 'zone', jsonb_build_array(
                                       jsonb_build_object('type', 'Property', 'value', 'b'))),
                ARRAY[$2 || t], now(), now()
           FROM unnest(ARRAY['Car','Bus','Stop','Tree','Lamp']) AS t,
                generate_series(1, 20000) AS n
         UNION ALL
         SELECT $1, 'urn:ngsi-ld:Car:zone-a',
                jsonb_build_object('id', 'urn:ngsi-ld:Car:zone-a',
                                   'type', jsonb_build_array($2 || 'Tree', $2 || 'Car'),
                                   $2 || 'zone', jsonb_build_array(
                                       jsonb_build_object('type', 'Property', 'value', 'a'))),
                ARRAY[$2 || 'Tree', $2 || 'Car'], now(), now()",
    )
    .bind(&tenant)
    .bind(DEFAULT)
    .execute(&mut *tx)
    .await
    .expect("entities");
    tx.commit().await.expect("commit");
    antares_sql::sqlx::query("ANALYZE entities")
        .execute(&pool)
        .await
        .expect("analyze");

    let store = antares_sql::store::any::AnyStore::Pg(antares_sql::store::any::PgBackend::new(
        pool.clone(),
    ));
    let mut st = AppState::with_store("me".into(), Arc::new(store), "postgres")
        .with_policy(Arc::new(Narrowing(view())));
    antares_api::wire(&mut st).await;

    // the first call warms the connections; the best of the next five is
    // measured, so a shared CI runner's one slow moment is not the claim
    let (code, body, _) = types(&st, &tenant).await;
    assert_eq!(code, StatusCode::OK, "{body}");
    let mut best = Duration::MAX;
    for _ in 0..5 {
        let (code, body, took) = types(&st, &tenant).await;
        assert_eq!(code, StatusCode::OK, "{body}");
        assert_eq!(
            body["typeList"],
            serde_json::json!(["Bus", "Car", "Tree"]),
            "{body}"
        );
        best = best.min(took);
    }
    assert!(
        best < Duration::from_millis(200),
        "/types took {best:?} at best of 5"
    );

    antares_sql::sqlx::query("DELETE FROM entities WHERE tenant_id = $1")
        .bind(&tenant)
        .execute(&pool)
        .await
        .expect("clean");
}

/// `GET /types?details=true` folds every Entity (5.7.5 gives attributeNames,
/// which no catalogue keeps). Over 100 000 Entities the fold reads each row
/// once by keyset (T-4683): with OFFSET paging every page re-read the rows
/// before it, 57-59 s on one prod tenant, holding three pool sessions. Local
/// debug build: 11 s by OFFSET, under 2 s by keyset; the bound sits between.
#[tokio::test(flavor = "multi_thread")]
async fn type_details_over_100k_entities_read_each_row_once() {
    let Ok(url) = std::env::var("ANTARES_TEST_DATABASE_URL") else {
        eprintln!("SKIP: ANTARES_TEST_DATABASE_URL not set");
        return;
    };
    let pool = antares_sql::store::pg::connect(&url, 5)
        .await
        .expect("connect");
    let tenant = format!("td{}", &uuid::Uuid::new_v4().simple().to_string()[..12]);
    let tid = TenantId::new(&tenant).expect("tenant");
    antares_sql::store::pg::ensure_tenant(&pool, &tid)
        .await
        .expect("tenant row");
    let mut tx = pool.begin().await.expect("tx");
    antares_sql::sqlx::query("SELECT set_config('antares.tenant', $1, true)")
        .bind(&tenant)
        .execute(&mut *tx)
        .await
        .expect("tenant setting");
    antares_sql::sqlx::query(
        "INSERT INTO entities (tenant_id, id, entity, types, created_at, modified_at)
         SELECT $1, 'urn:ngsi-ld:' || t || ':' || lpad(n::text, 6, '0'),
                jsonb_build_object('id', 'urn:ngsi-ld:' || t || ':' || lpad(n::text, 6, '0'),
                                   'type', jsonb_build_array($2 || t),
                                   $2 || 'zone', jsonb_build_array(
                                       jsonb_build_object('type', 'Property', 'value', 'b'))),
                ARRAY[$2 || t], now(), now()
           FROM unnest(ARRAY['Car','Bus','Stop','Tree','Lamp']) AS t,
                generate_series(1, 20000) AS n",
    )
    .bind(&tenant)
    .bind(DEFAULT)
    .execute(&mut *tx)
    .await
    .expect("entities");
    tx.commit().await.expect("commit");
    antares_sql::sqlx::query("ANALYZE entities")
        .execute(&pool)
        .await
        .expect("analyze");

    let store = antares_sql::store::any::AnyStore::Pg(antares_sql::store::any::PgBackend::new(
        pool.clone(),
    ));
    let mut st = AppState::with_store("me".into(), Arc::new(store), "postgres");
    antares_api::wire(&mut st).await;

    let (code, body, took) = types_at(&st, &tenant, "/ngsi-ld/v1/types?details=true").await;
    assert_eq!(code, StatusCode::OK, "{body}");
    let names: Vec<&str> = body
        .as_array()
        .expect("an EntityType array")
        .iter()
        .filter_map(|t| t["typeName"].as_str())
        .collect();
    assert_eq!(names, ["Bus", "Car", "Lamp", "Stop", "Tree"], "{body}");
    assert!(
        body.as_array()
            .into_iter()
            .flatten()
            .all(|t| t["attributeNames"] == serde_json::json!(["zone"])),
        "{body}"
    );
    eprintln!("details over 100 000 Entities: {took:?}");
    assert!(took < Duration::from_secs(5), "details took {took:?}");

    antares_sql::sqlx::query("DELETE FROM entities WHERE tenant_id = $1")
        .bind(&tenant)
        .execute(&pool)
        .await
        .expect("clean");
}
