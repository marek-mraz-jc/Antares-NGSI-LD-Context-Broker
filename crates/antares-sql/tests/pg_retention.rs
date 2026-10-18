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
use antares_sql::store::pg::maintenance::{
    history_overrides, retain_current, retain_history, temporal_maintenance, TemporalBackend,
};
use antares_sql::store::pg::temporal::PgTemporalStore;
use antares_store::{HistoryScope, HISTORY_RETENTION};
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

fn ty(type_iri: &str) -> HistoryScope<'_> {
    HistoryScope::Type {
        type_iri,
        except_ids: &[],
    }
}

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
        retain_history(&pool, "retain-h", ty(V), &ago(5), 1000, true)
            .await
            .expect("dry run"),
        1
    );
    let n = retain_history(&pool, "retain-h", ty(V), &ago(5), 1000, false)
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
        let n = retain_history(&pool, "retain-h", ty(V), &ago(5), 1000, false)
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
        retain_history(&app, "retain-rls", ty(V), &ago(5), 1000, true)
            .await
            .expect("dry run"),
        1,
        "the dry run sees the old instance through the tenant policy"
    );
    assert_eq!(
        retain_history(&app, "retain-rls", ty(V), &ago(5), 1000, false)
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

/// DM-97 on the Postgres arm: one Entity, a type without the Entities that
/// rule themselves, the rest of a Tenant without its ruled types; the
/// Entities with their own `historyRetention`, read across Tenants.
#[tokio::test(flavor = "multi_thread")]
async fn history_scopes_reach_one_entity_a_type_and_the_rest() {
    let url = require_db!();
    let pool = pg::connect(&url, 5).await.expect("connect");
    let store = PgTemporalStore::new(pool.clone());
    let attr = "https://example.org/speed";
    let other = "https://example.org/Other";
    let tenant = TenantId::new("retain-scope").expect("tenant");
    pg::ensure_tenant(&pool, &tenant).await.expect("tenant row");
    let ids = [
        ("urn:v:own", V),
        ("urn:v:plain", V),
        ("urn:w:plain", W),
        ("urn:x:plain", other),
    ];
    for (id, ty) in ids {
        let _ = store.delete(&tenant, id).await;
        let doc = json!({
            "id": id, "type": [ty], "createdAt": ago(30), "modifiedAt": ago(10),
            attr: [{"type": "Property", "value": 1, "observedAt": ago(10), "instanceId": format!("urn:i:{id}")}]
        });
        assert!(store.create(&tenant, id, &doc).await.expect("create"));
    }
    let left = |id: &'static str| {
        let pool = pool.clone();
        async move {
            sqlx::query_scalar::<_, i64>(
                "SELECT count(*) FROM attr_instances WHERE tenant_id = 'retain-scope' AND entity_id = $1",
            )
            .bind(id)
            .fetch_one(&pool)
            .await
            .expect("count")
        }
    };
    let own = ["urn:v:own".to_owned()];
    let kept = HistoryScope::Type {
        type_iri: V,
        except_ids: &own,
    };
    assert_eq!(
        retain_history(&pool, "retain-scope", kept, &ago(5), 100, false)
            .await
            .expect("type"),
        1
    );
    assert_eq!((left("urn:v:own").await, left("urn:v:plain").await), (1, 0));
    let ruled = [W.to_owned()];
    let rest = HistoryScope::Rest {
        except_types: &ruled,
        except_ids: &own,
    };
    assert_eq!(
        retain_history(&pool, "retain-scope", rest, &ago(5), 100, true)
            .await
            .expect("dry"),
        1
    );
    assert_eq!(
        retain_history(&pool, "retain-scope", rest, &ago(5), 100, false)
            .await
            .expect("rest"),
        1
    );
    assert_eq!(
        (
            left("urn:w:plain").await,
            left("urn:x:plain").await,
            left("urn:v:own").await
        ),
        (1, 0, 1)
    );
    let one = HistoryScope::Entity("urn:v:own");
    assert_eq!(
        retain_history(&pool, "retain-scope", one, &ago(5), 100, false)
            .await
            .expect("entity"),
        1
    );
    assert_eq!((left("urn:v:own").await, left("urn:w:plain").await), (0, 1));

    for (t, id, value) in [
        ("retain-owna", "urn:v:a", Some("P7D")),
        ("retain-ownb", "urn:v:b", Some("off")),
        ("retain-ownb", "urn:v:c", None),
    ] {
        sqlx::query("DELETE FROM entities WHERE tenant_id = $1 AND id = $2")
            .bind(t)
            .bind(id)
            .execute(&pool)
            .await
            .expect("clean");
        let mut doc = json!({"id": id});
        if let Some(v) = value {
            doc[HISTORY_RETENTION] = json!([{"type": "Property", "value": v}]);
        }
        sqlx::query(
            "INSERT INTO entities (tenant_id, id, entity, types, created_at, modified_at)
             VALUES ($1, $2, $3, ARRAY[$4], now(), now())",
        )
        .bind(t)
        .bind(id)
        .bind(&doc)
        .bind(V)
        .execute(&pool)
        .await
        .expect("entity");
    }
    let found: Vec<(String, String, String)> = history_overrides(&pool, 10_000)
        .await
        .expect("overrides")
        .into_iter()
        .filter(|(t, _, _)| t.starts_with("retain-own"))
        .map(|(t, id, own)| {
            (
                t,
                id,
                own[0]["value"].as_str().unwrap_or_default().to_owned(),
            )
        })
        .collect();
    let mut found = found;
    found.sort();
    assert_eq!(
        found,
        [
            (
                "retain-owna".to_owned(),
                "urn:v:a".to_owned(),
                "P7D".to_owned()
            ),
            (
                "retain-ownb".to_owned(),
                "urn:v:b".to_owned(),
                "off".to_owned()
            )
        ]
    );
}

/// DM-97: a weekly partition below the pre-created window that the sweeps
/// emptied is dropped, so its disk goes back; one that still holds a row,
/// and the window itself, stay.
#[tokio::test(flavor = "multi_thread")]
async fn a_past_partition_the_sweeps_emptied_is_dropped_and_a_held_one_stays() {
    let url = require_db!();
    let pool = pg::connect(&url, 5).await.expect("connect");
    if pg::maintenance::detect_temporal_backend(&pool).await != Ok(TemporalBackend::Partitioned) {
        eprintln!("SKIP: attr_instances is not natively partitioned here");
        return;
    }
    let tenant = TenantId::new("retain-part").expect("tenant");
    pg::ensure_tenant(&pool, &tenant).await.expect("tenant row");
    let exists = |name: &'static str| {
        let pool = pool.clone();
        async move {
            sqlx::query_scalar::<_, bool>(
                "SELECT EXISTS (SELECT 1 FROM pg_class WHERE relname = $1 AND relnamespace = 'public'::regnamespace)",
            )
            .bind(name)
            .fetch_one(&pool)
            .await
            .expect("catalog")
        }
    };
    let store = PgTemporalStore::new(pool.clone());
    // A row an earlier run left in DEFAULT would block the week's partition.
    let _ = store.delete(&tenant, "urn:v:held").await;
    // Hold the maintenance claim while the weeks are made and one is filled,
    // so a parallel test's run cannot drop the held week while it is empty.
    let mut claim = pool.begin().await.expect("claim");
    sqlx::query("SELECT name FROM maintenance_jobs WHERE name = 'temporal_partitions' FOR UPDATE")
        .fetch_optional(&mut *claim)
        .await
        .expect("claim row");
    for (name, lo, hi) in [
        ("attr_instances_2001w02", "2001-01-08", "2001-01-15"),
        ("attr_instances_2001w03", "2001-01-15", "2001-01-22"),
    ] {
        sqlx::query(sqlx::AssertSqlSafe(format!(
            "CREATE TABLE IF NOT EXISTS {name} PARTITION OF attr_instances FOR VALUES FROM ('{lo}') TO ('{hi}')"
        )))
        .execute(&pool)
        .await
        .expect("partition");
    }
    let doc = json!({
        "id": "urn:v:held", "type": [V], "createdAt": "2001-01-16T00:00:00Z", "modifiedAt": "2001-01-16T00:00:00Z",
        "https://example.org/speed": [{"type": "Property", "value": 1, "observedAt": "2001-01-16T00:00:00Z", "instanceId": "urn:i:held"}]
    });
    assert!(store
        .create(&tenant, "urn:v:held", &doc)
        .await
        .expect("create"));
    claim.commit().await.expect("release");
    // Another test's maintenance may hold the claim; the winner drops.
    let mut done = String::new();
    for _ in 0..50 {
        done = temporal_maintenance(&pool, TemporalBackend::Partitioned, None)
            .await
            .expect("maintenance");
        if !done.starts_with("skipped") {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }
    assert!(
        !exists("attr_instances_2001w02").await,
        "the empty week is dropped: {done}"
    );
    assert!(
        exists("attr_instances_2001w03").await,
        "the held week stays: {done}"
    );
    let this_week: String =
        sqlx::query_scalar("SELECT 'attr_instances_' || to_char(now(), 'IYYY\"w\"IW')")
            .fetch_one(&pool)
            .await
            .expect("name");
    let window: bool = sqlx::query_scalar(
        "SELECT EXISTS (SELECT 1 FROM pg_class WHERE relname = $1 AND relnamespace = 'public'::regnamespace)",
    )
    .bind(&this_week)
    .fetch_one(&pool)
    .await
    .expect("catalog");
    assert!(window, "the current week stays though it may be empty");
    let _ = store.delete(&tenant, "urn:v:held").await;
}

/// DM-97, owner 2026-10-18: an Entity its `expiresAt` ends takes its history
/// with it; an Entity of the same Tenant without one keeps its own.
#[tokio::test(flavor = "multi_thread")]
async fn an_expired_entity_goes_with_its_history() {
    let url = require_db!();
    let pool = pg::connect(&url, 5).await.expect("connect");
    let store = PgTemporalStore::new(pool.clone());
    let tenant = TenantId::new("retain-exp").expect("tenant");
    pg::ensure_tenant(&pool, &tenant).await.expect("tenant row");
    for id in ["urn:v:exp", "urn:v:stays"] {
        let _ = store.delete(&tenant, id).await;
        sqlx::query("DELETE FROM entities WHERE tenant_id = 'retain-exp' AND id = $1")
            .bind(id)
            .execute(&pool)
            .await
            .expect("clean");
        let doc = json!({
            "id": id, "type": [V], "createdAt": ago(30), "modifiedAt": ago(10),
            "https://example.org/speed": [{"type": "Property", "value": 1, "observedAt": ago(10), "instanceId": format!("urn:i:{id}")}]
        });
        assert!(store.create(&tenant, id, &doc).await.expect("create"));
    }
    sqlx::query(
        "INSERT INTO entities (tenant_id, id, entity, types, created_at, modified_at, expires_at)
         VALUES ('retain-exp', 'urn:v:exp', '{\"id\":\"urn:v:exp\"}', ARRAY[$1], now(), now(), now() - interval '1 minute'),
                ('retain-exp', 'urn:v:stays', '{\"id\":\"urn:v:stays\"}', ARRAY[$1], now(), now(), NULL)",
    )
    .bind(V)
    .execute(&pool)
    .await
    .expect("entities");
    let mut done = String::new();
    for _ in 0..50 {
        done = temporal_maintenance(&pool, TemporalBackend::Partitioned, None)
            .await
            .expect("maintenance");
        if !done.starts_with("skipped") {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }
    let rows = |sql: &'static str| {
        let pool = pool.clone();
        async move {
            sqlx::query_scalar::<_, String>(sql)
                .fetch_all(&pool)
                .await
                .expect("rows")
        }
    };
    assert_eq!(
        rows("SELECT id FROM entities WHERE tenant_id = 'retain-exp' ORDER BY id").await,
        ["urn:v:stays"],
        "{done}"
    );
    assert_eq!(
        rows("SELECT DISTINCT entity_id FROM attr_instances WHERE tenant_id = 'retain-exp' ORDER BY 1").await,
        ["urn:v:stays"]
    );
    let shells = store.get(&tenant, "urn:v:exp").await.expect("read");
    assert!(shells.is_none(), "the temporal Entity is gone too");
}
