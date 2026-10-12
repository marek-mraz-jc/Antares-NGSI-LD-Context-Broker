// SPDX-License-Identifier: EUPL-1.2
//! Retention per Tenant and type on the Postgres arm (`/q/retention`,
//! T-3900): `retain_current` removes the Entities of one type in one Tenant
//! not modified since the cut and carrying no `expiresAt` of their own;
//! `retain_history` removes that type's instances observed before the cut;
//! both stop at the batch they are given. Skips loudly without
//! ANTARES_TEST_DATABASE_URL (see tests/pg.rs recipe).

#![cfg(feature = "postgres")]

use antares_model::TenantId;
use antares_sql::store::pg;
use antares_sql::store::pg::maintenance::{retain_current, retain_history};
use antares_sql::store::pg::temporal::PgTemporalStore;
use serde_json::json;

macro_rules! require_db {
    () => {
        match std::env::var("ANTARES_TEST_DATABASE_URL") {
            Ok(url) => url,
            Err(_) => {
                eprintln!("SKIP: ANTARES_TEST_DATABASE_URL not set");
                return;
            }
        }
    };
}

const V: &str = "https://example.org/Vehicle";
const W: &str = "https://example.org/Stop";

fn ago(minutes: i64) -> String {
    (chrono::Utc::now() - chrono::Duration::minutes(minutes))
        .to_rfc3339_opts(chrono::SecondsFormat::Millis, true)
}

async fn entity(
    pool: &sqlx::PgPool,
    tenant: &str,
    id: &str,
    ty: &str,
    modified_min_ago: i64,
    expires: bool,
) {
    sqlx::query(
        "INSERT INTO entities (tenant_id, id, entity, types, created_at, modified_at, expires_at)
         VALUES ($1, $2, jsonb_build_object('id', $2::text), ARRAY[$3], now() - make_interval(mins => $4::int),
                 now() - make_interval(mins => $4::int), CASE WHEN $5 THEN now() + interval '1 day' END)",
    )
    .bind(tenant)
    .bind(id)
    .bind(ty)
    .bind(i32::try_from(modified_min_ago).expect("minutes"))
    .bind(expires)
    .execute(pool)
    .await
    .expect("entity row");
}

async fn count(pool: &sqlx::PgPool, sql: &'static str, tenant: &str) -> i64 {
    sqlx::query_scalar(sql)
        .bind(tenant)
        .fetch_one(pool)
        .await
        .expect("count")
}

#[tokio::test(flavor = "multi_thread")]
async fn current_state_of_one_type_in_one_tenant_goes_and_a_writers_expiry_wins() {
    let url = require_db!();
    let pool = pg::connect(&url, 5).await.expect("connect");
    for t in ["retain-a", "retain-b"] {
        pg::ensure_tenant(&pool, &TenantId::new(t).expect("tenant"))
            .await
            .expect("tenant row");
        sqlx::query("DELETE FROM entities WHERE tenant_id = $1")
            .bind(t)
            .execute(&pool)
            .await
            .expect("clean");
    }
    entity(&pool, "retain-a", "urn:v:old", V, 10, false).await;
    entity(&pool, "retain-a", "urn:v:fresh", V, 1, false).await;
    entity(&pool, "retain-a", "urn:v:own-expiry", V, 10, true).await;
    entity(&pool, "retain-a", "urn:w:old", W, 10, false).await;
    entity(&pool, "retain-b", "urn:v:old", V, 10, false).await;

    assert_eq!(
        retain_current(&pool, "retain-a", V, &ago(5), 1000, true)
            .await
            .expect("dry run"),
        1,
        "a dry run counts the same row"
    );
    let n = retain_current(&pool, "retain-a", V, &ago(5), 1000, false)
        .await
        .expect("retain");
    assert_eq!(n, 1, "only the old Vehicle without its own expiresAt");
    let left: Vec<String> =
        sqlx::query_scalar("SELECT id FROM entities WHERE tenant_id = 'retain-a' ORDER BY id")
            .fetch_all(&pool)
            .await
            .expect("ids");
    assert_eq!(left, ["urn:v:fresh", "urn:v:own-expiry", "urn:w:old"]);
    assert_eq!(
        count(
            &pool,
            "SELECT count(*) FROM entities WHERE tenant_id = $1",
            "retain-b"
        )
        .await,
        1,
        "tenant B untouched"
    );
    assert_eq!(
        retain_current(&pool, "retain-a", V, &ago(5), 1000, false)
            .await
            .expect("again"),
        0,
        "idempotent"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn history_of_one_type_goes_before_the_cut_in_bounded_batches() {
    let url = require_db!();
    let pool = pg::connect(&url, 5).await.expect("connect");
    let store = PgTemporalStore::new(pool.clone());
    let attr = "https://example.org/speed";
    for t in ["retain-h", "retain-i"] {
        let tenant = TenantId::new(t).expect("tenant");
        pg::ensure_tenant(&pool, &tenant).await.expect("tenant row");
        for id in ["urn:v:1", "urn:w:1", "urn:v:backlog"] {
            let _ = store.delete(&tenant, id).await;
        }
        for (id, ty) in [("urn:v:1", V), ("urn:w:1", W)] {
            let doc = json!({
                "id": id, "type": [ty], "createdAt": ago(30), "modifiedAt": ago(4),
                attr: [
                    {"type": "Property", "value": 1, "observedAt": ago(6), "instanceId": format!("urn:i:{id}:6")},
                    {"type": "Property", "value": 2, "observedAt": ago(4), "instanceId": format!("urn:i:{id}:4")}]
            });
            assert!(store.create(&tenant, id, &doc).await.expect("create"));
        }
    }
    assert_eq!(
        retain_history(&pool, "retain-h", V, &ago(5), 1000, true)
            .await
            .expect("dry run"),
        1
    );
    let n = retain_history(&pool, "retain-h", V, &ago(5), 1000, false)
        .await
        .expect("retain");
    assert_eq!(n, 1, "the Vehicle's 6-minute instance");
    let per = |t: &'static str| {
        let pool = pool.clone();
        async move {
            sqlx::query_as::<_, (String, i64)>(
                "SELECT entity_id, count(*) FROM attr_instances WHERE tenant_id = $1 GROUP BY entity_id ORDER BY entity_id",
            )
            .bind(t)
            .fetch_all(&pool)
            .await
            .expect("counts")
        }
    };
    assert_eq!(
        per("retain-h").await,
        [("urn:v:1".to_owned(), 1), ("urn:w:1".to_owned(), 2)],
        "the Stop keeps everything"
    );
    assert_eq!(
        per("retain-i").await,
        [("urn:v:1".to_owned(), 2), ("urn:w:1".to_owned(), 2)],
        "tenant I untouched"
    );

    // A backlog drains in statements of at most the batch.
    let tenant = TenantId::new("retain-h").expect("tenant");
    let instances: Vec<_> = (0..2500)
        .map(|i| json!({"type": "Property", "value": i, "observedAt": ago(60 + i64::from(i % 50)), "instanceId": format!("urn:i:b:{i}")}))
        .collect();
    let doc = json!({"id": "urn:v:backlog", "type": [V], "createdAt": ago(120), "modifiedAt": ago(60), attr: instances});
    assert!(store
        .create(&tenant, "urn:v:backlog", &doc)
        .await
        .expect("backlog"));
    let mut rounds = Vec::new();
    loop {
        let n = retain_history(&pool, "retain-h", V, &ago(5), 1000, false)
            .await
            .expect("retain");
        rounds.push(n);
        if n < 1000 {
            break;
        }
    }
    assert_eq!(rounds, [1000, 1000, 500]);
}

/// The broker connects as a role that is neither owner nor BYPASSRLS, so
/// `temporal_entities`' tenant policy applies to the sweep's JOIN: with only
/// the service flag set, the JOIN saw no row and history retention deleted
/// nothing on dev while reporting `Ok(0)` (T-3900 re-open). Run as such a
/// role, the history and current-state rules both remove what they select.
#[tokio::test(flavor = "multi_thread")]
async fn retention_deletes_as_the_brokers_rls_bound_role() {
    let url = require_db!();
    let admin = pg::connect(&url, 5).await.expect("connect");
    for stmt in [
        "DO $$ BEGIN CREATE ROLE antares_app LOGIN PASSWORD 'app';
         EXCEPTION WHEN duplicate_object OR unique_violation THEN NULL; END $$",
        "GRANT USAGE ON SCHEMA public TO antares_app",
        "GRANT SELECT, INSERT, UPDATE, DELETE ON ALL TABLES IN SCHEMA public TO antares_app",
    ] {
        sqlx::query(stmt).execute(&admin).await.expect(stmt);
    }
    let app_url = url.replace("antares:antares@", "antares_app:app@");
    assert_ne!(
        app_url, url,
        "test URL must embed antares:antares credentials"
    );
    let app = sqlx::postgres::PgPoolOptions::new()
        .max_connections(2)
        .connect(&app_url)
        .await
        .expect("app-role connect");
    let bypass: bool = sqlx::query_scalar(
        "SELECT rolsuper OR rolbypassrls FROM pg_roles WHERE rolname = current_user",
    )
    .fetch_one(&app)
    .await
    .expect("role");
    assert!(
        !bypass,
        "the role must wear RLS or this test proves nothing"
    );

    let store = PgTemporalStore::new(admin.clone());
    let attr = "https://example.org/speed";
    let tenant = TenantId::new("retain-rls").expect("tenant");
    pg::ensure_tenant(&admin, &tenant)
        .await
        .expect("tenant row");
    let _ = store.delete(&tenant, "urn:v:rls").await;
    sqlx::query("DELETE FROM entities WHERE tenant_id = 'retain-rls'")
        .execute(&admin)
        .await
        .expect("clean");
    let doc = json!({
        "id": "urn:v:rls", "type": [V], "createdAt": ago(30), "modifiedAt": ago(4),
        attr: [
            {"type": "Property", "value": 1, "observedAt": ago(6), "instanceId": "urn:i:rls:6"},
            {"type": "Property", "value": 2, "observedAt": ago(4), "instanceId": "urn:i:rls:4"}]
    });
    assert!(store
        .create(&tenant, "urn:v:rls", &doc)
        .await
        .expect("create"));
    entity(&admin, "retain-rls", "urn:v:rls-old", V, 10, false).await;

    assert_eq!(
        retain_history(&app, "retain-rls", V, &ago(5), 1000, true)
            .await
            .expect("dry run"),
        1,
        "the dry run sees the old instance through the tenant policy"
    );
    assert_eq!(
        retain_history(&app, "retain-rls", V, &ago(5), 1000, false)
            .await
            .expect("retain history"),
        1,
        "the 6-minute instance goes"
    );
    assert_eq!(
        count(
            &admin,
            "SELECT count(*) FROM attr_instances WHERE tenant_id = $1",
            "retain-rls"
        )
        .await,
        1,
        "the 4-minute instance stays"
    );
    assert_eq!(
        retain_current(&app, "retain-rls", V, &ago(5), 1000, false)
            .await
            .expect("retain current"),
        1,
        "the stale Vehicle goes too"
    );
}
