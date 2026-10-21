// SPDX-License-Identifier: EUPL-1.2
//! EXPLAIN-based index-usability regression tests: a review once
//! found three of five entity indexes dead — the compiled q= used the
//! function form `jsonb_path_exists()` the GIN index can never match, and the
//! geo predicate's `location IS NULL OR …` guard defeated the GIST index.
//! These tests pin the repaired shapes as the broker's own role plans them
//! (T-4852): `antares_app` under the tables' forced RLS, sequential scans
//! allowed, over two seeded Tenants of 100 000 Entities each with fresh
//! statistics. Under RLS only a leakproof operator becomes an index
//! condition, so a superuser plan (RLS skipped) proves nothing about the
//! broker: a walk of the Tenant there means the index went dead again.
//!
//! Literal values are inlined (EXPLAIN cannot carry extended-protocol binds);
//! this is test-only SQL, never a pattern for request paths.

// Every test here talks to the Postgres store, which only exists with the
// `postgres` feature; without it the file builds to nothing (T-3465).
#![cfg(feature = "postgres")]

use antares_sql::store::pg;

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

/// The plan the broker's role gets: `antares_app` (created and granted as
/// `pg_rls_pentest.rs` does), the Tenant set, sequential scans allowed.
/// `service` arms the 4.22 sweep's cross-tenant escape instead.
async fn plan_of(pool: &sqlx::PgPool, tenant: &str, sql: &str) -> String {
    plan_as(pool, tenant, false, sql).await
}

async fn plan_as(pool: &sqlx::PgPool, tenant: &str, service: bool, sql: &str) -> String {
    app_role(pool).await;
    let mut tx = pool.begin().await.expect("tx");
    sqlx::query("SET LOCAL ROLE antares_app")
        .execute(&mut *tx)
        .await
        .expect("role");
    sqlx::query(
        "SELECT set_config('antares.tenant', $1, true), set_config('antares.service', $2, true)",
    )
    .bind(tenant)
    .bind(if service { "on" } else { "off" })
    .execute(&mut *tx)
    .await
    .expect("tenant");
    let rows: Vec<String> =
        sqlx::query_scalar(sqlx::AssertSqlSafe(format!("EXPLAIN (FORMAT TEXT) {sql}")))
            .fetch_all(&mut *tx)
            .await
            .expect("explain");
    rows.join("\n")
}

/// `antares_app`, neither superuser nor BYPASSRLS, granted the tables.
/// Role DDL is cluster-global: serialized by advisory lock 4334, which every
/// antares-sql test that grants the role takes (T-4978).
async fn app_role(pool: &sqlx::PgPool) {
    let mut tx = pool.begin().await.expect("tx");
    sqlx::query("SET LOCAL lock_timeout = 0")
        .execute(&mut *tx)
        .await
        .expect("no lock timeout: the seed holds the lock for seconds");
    sqlx::query("SELECT pg_advisory_xact_lock(4334)")
        .execute(&mut *tx)
        .await
        .expect("lock");
    for stmt in [
        "DO $$ BEGIN CREATE ROLE antares_app LOGIN PASSWORD 'app';
         EXCEPTION WHEN duplicate_object OR unique_violation THEN NULL; END $$",
        "GRANT USAGE ON SCHEMA public TO antares_app",
        "GRANT SELECT, INSERT, UPDATE, DELETE ON ALL TABLES IN SCHEMA public TO antares_app",
    ] {
        sqlx::query(stmt).execute(&mut *tx).await.expect(stmt);
    }
    tx.commit().await.expect("grants");
    let bypass: bool = sqlx::query_scalar(
        "SELECT rolbypassrls OR rolsuper FROM pg_roles WHERE rolname = 'antares_app'",
    )
    .fetch_one(pool)
    .await
    .expect("role flags");
    assert!(
        !bypass,
        "antares_app must not bypass RLS or no plan here proves anything"
    );
}

/// Entities per seeded Tenant, the scale the plans are asserted at.
const ROWS: i64 = 100_000;
const RLS_A: &str = "pgexplain-rls-a";
const RLS_B: &str = "pgexplain-rls-b";

/// Two Tenants of `ROWS` Entities each, identical ids, `https://x/tenant`
/// telling them apart: ten types `https://x/T0..T9` (1 %, all of T0,
/// also `https://x/M`), `speed` 0..99, `brand` b0..b49, a point on a 10° x 1°
/// grid near (15, 45.5); the same ids as temporal Entities, the first 1000
/// with 50 `speed` instances each. Seeded once per database and `ANALYZE`d
/// inside the seeding transaction, so no test plans on stale statistics.
async fn seed_rls(pool: &sqlx::PgPool) {
    let mut tx = pool.begin().await.expect("tx");
    sqlx::query("SET LOCAL lock_timeout = 0")
        .execute(&mut *tx)
        .await
        .expect("no lock timeout: the seed holds the lock for seconds");
    sqlx::query("SELECT pg_advisory_xact_lock(48520)")
        .execute(&mut *tx)
        .await
        .expect("lock");
    let have: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM attr_instances WHERE tenant_id = $1 AND entity_id = 'urn:ngsi-ld:T0:00001000'",
    )
    .bind(RLS_B)
    .fetch_one(&mut *tx)
    .await
    .expect("seeded?");
    if have == 50 {
        return;
    }
    for t in [RLS_A, RLS_B] {
        for stmt in [
            "DELETE FROM entities WHERE tenant_id = $1",
            "DELETE FROM temporal_entities WHERE tenant_id = $1",
            "DELETE FROM attr_instances WHERE tenant_id = $1",
            "INSERT INTO tenants (tenant_id) VALUES ($1) ON CONFLICT DO NOTHING",
            "INSERT INTO entities (tenant_id, id, entity, types, location, created_at, modified_at)
             SELECT $1, 'urn:ngsi-ld:T' || (g % 10) || ':' || lpad(g::text, 8, '0'),
                    jsonb_build_object(
                      'https://x/tenant', $1,
                      'https://x/speed', jsonb_build_array(jsonb_build_object('type', 'Property', 'value', g % 100)),
                      'https://x/brand', jsonb_build_array(jsonb_build_object('type', 'Property', 'value', 'b' || (g % 50)))),
                    CASE WHEN g % 100 = 0 THEN ARRAY['https://x/T' || (g % 10), 'https://x/M']
                         ELSE ARRAY['https://x/T' || (g % 10)] END,
                    ST_SetSRID(ST_MakePoint(10 + (g % 1000) / 100.0, 45 + ((g / 1000) % 100) / 100.0), 4326),
                    now(), now()
               FROM generate_series(1, $2::bigint) g",
            "INSERT INTO temporal_entities (tenant_id, id, types, meta, created_at, modified_at)
             SELECT tenant_id, id, types, jsonb_build_object('id', id, 'https://x/tenant', $1), now(), now()
               FROM entities WHERE tenant_id = $1 AND $2::bigint > 0",
            "INSERT INTO attr_instances (tenant_id, entity_id, attr_id, instance_id, observed_at,
                                         created_at, modified_at, data)
             SELECT $1, 'urn:ngsi-ld:T' || (g % 10) || ':' || lpad(g::text, 8, '0'), 'https://x/speed',
                    'urn:i:' || k, now() - k * interval '1 minute', now(), now(),
                    jsonb_build_object('type', 'Property', 'value', k)
               FROM generate_series(1, least($2::bigint, 1000)) g, generate_series(1, 50) k",
        ] {
            sqlx::query(stmt)
                .bind(t)
                .bind(ROWS)
                .execute(&mut *tx)
                .await
                .expect(stmt);
        }
    }
    // statistics before the lock goes: a test waiting on it plans at once
    sqlx::query("ANALYZE entities, temporal_entities, attr_instances")
        .execute(&mut *tx)
        .await
        .expect("analyze");
    tx.commit().await.expect("seed");
}

/// The type arms the store reads (`store::pg::entity::arms`, whose unit
/// tests pin the text), for the expanded type groups `groups`.
fn type_page(w: &str, groups: &[&[&str]], limit: i64) -> String {
    let lit = |t: &str| format!("'{t}'");
    let arr = |ts: &[&str]| {
        format!(
            "ARRAY[{}]",
            ts.iter().map(|t| lit(t)).collect::<Vec<_>>().join(", ")
        )
    };
    let mut named: Vec<&str> = groups.iter().flat_map(|g| g.iter().copied()).collect();
    named.sort_unstable();
    named.dedup();
    let selection = format!(
        "({})",
        groups
            .iter()
            .map(|g| format!("types @> {}", arr(g)))
            .collect::<Vec<_>>()
            .join(" OR ")
    );
    let exact = matches!(groups, [[_]]);
    let mut arms: Vec<String> = named
        .iter()
        .map(|t| match exact {
            true => format!("{w} AND types[1] = {}", lit(t)),
            false => format!("{w} AND types[1] = {} AND {selection}", lit(t)),
        })
        .collect();
    arms.push(format!(
        "{w} AND cardinality(types) > 1 AND types[1] <> ALL({}) AND {selection}",
        arr(&named)
    ));
    let parts: Vec<String> = arms
        .iter()
        .map(|a| format!("(SELECT id, entity FROM entities WHERE {a} ORDER BY id LIMIT {limit})"))
        .collect();
    format!(
        "SELECT entity AS entity FROM ({}) AS entities ORDER BY id LIMIT {limit}",
        parts.join(" UNION ALL ")
    )
}

/// `sql` with its `$n` placeholders replaced by the literals `binds[n - 1]`,
/// highest first so `$1` never eats `$10`'s prefix. Test-only: EXPLAIN
/// takes no extended-protocol binds.
fn inline(sql: &str, binds: &[String]) -> String {
    let mut out = sql.to_owned();
    for (k, b) in binds.iter().enumerate().rev() {
        out = out.replace(&format!("${}", k + 1), b);
    }
    out
}

/// The WHERE a geoquery compiles to, with its binds inlined.
fn geo_where(
    rel: antares_sql::compile::geo::Rel,
    geometry: &str,
    coordinates: &serde_json::Value,
) -> String {
    use antares_sql::compile::geo::{compile_geo, GeoSpec};
    let c = compile_geo(
        &GeoSpec {
            rel,
            geometry,
            coordinates,
            geoproperty_iri: "",
        },
        "location",
        1,
    )
    .expect("geo compiles");
    let text = |b: &String| format!("'{}'", b.replace('\'', "''"));
    let binds: Vec<String> = (c.geo_binds.iter().map(text))
        .chain(c.num_binds.iter().map(f64::to_string))
        .chain(c.cell_binds.iter().map(text))
        .collect();
    inline(&c.sql, &binds)
}

/// T-4852: every NGSI-LD query shape of `docs/src/performance.md`'s RLS
/// table, as the broker's role plans it beside a second Tenant of the same
/// size: each reads an index for its selective predicate, none walks the
/// Tenant (`entities_pkey` or `temporal_entities_pkey` with the predicate
/// left as a filter, a sequential scan, or a bitmap over the Tenant).
#[tokio::test]
async fn every_query_shape_reads_an_index_under_rls() {
    use antares_sql::compile::geo::Rel;
    let url = require_db!();
    let pool = pg::connect(&url, 5).await.expect("connect");
    seed_rls(&pool).await;
    let w = format!("tenant_id = '{RLS_A}' AND (expires_at IS NULL OR expires_at > now())");
    let speed = |cond: &str| {
        format!(r#"entity @? '$."https://x/speed"[*]."value" ? (@ {cond})'::jsonpath"#)
    };
    // A walk: a sequential scan, a primary key read with the predicate left
    // as a filter, or any other index whose condition is the Tenant alone,
    // except the partial indexes that hold a sliver of it by construction
    // (multi-type Entities, unextractable locations).
    let sliver = [
        "i_entities_multi_type_id",
        "i_entities_types_multi",
        "i_entities_loc_ambiguous",
        "i_temporal_entities_multi_type",
    ];
    let walks = |plan: &str| {
        let lines: Vec<&str> = plan.lines().collect();
        plan.contains("Seq Scan")
            || plan.contains("using entities_pkey")
            || plan.contains("using temporal_entities_pkey")
            || lines.windows(2).any(|l| {
                let tenant_only =
                    l[1].contains("Index Cond: (tenant_id =") && !l[1].contains(" AND ");
                tenant_only && !sliver.iter().any(|i| l[0].contains(i))
            })
    };
    let polygon = serde_json::json!([[
        [15.0, 45.5],
        [15.0, 45.52],
        [15.05, 45.52],
        [15.05, 45.5],
        [15.0, 45.5]
    ]]);
    let point = serde_json::json!([15.0, 45.5]);
    let t = |n: u8| format!("https://x/T{n}");
    let (t1, t2) = (t(1), t(2));
    let shapes: Vec<(&str, String, &str)> = vec![
        (
            "id",
            format!(
                "SELECT entity FROM entities WHERE {w} AND id = ANY(ARRAY['urn:ngsi-ld:T1:00000001', \
                 'urn:ngsi-ld:T2:00000002']) ORDER BY id LIMIT 21"
            ),
            "entities_pkey",
        ),
        ("type", type_page(&w, &[&[&t1]], 21), "i_entities_first_type_id"),
        ("type=A,B", type_page(&w, &[&[&t1], &[&t2]], 21), "i_entities_first_type_id"),
        ("type=A;B", type_page(&w, &[&[&t(0), "https://x/M"]], 21), "i_entities_first_type_id"),
        ("q equality", type_page(&format!("{w} AND {}", speed("== 42")), &[&[&t1]], 21), "i_entities_first_type_id"),
        ("q range", type_page(&format!("{w} AND {}", speed("> 90")), &[&[&t1]], 21), "i_entities_first_type_id"),
        (
            "attrs",
            type_page(&format!("{w} AND entity ?| ARRAY['https://x/brand']"), &[&[&t1]], 21),
            "i_entities_first_type_id",
        ),
        (
            "geo near",
            format!(
                "SELECT entity FROM entities WHERE {w} AND {} ORDER BY id LIMIT 10000",
                geo_where(Rel::Near { max: Some(500.0), min: None }, "Point", &point)
            ),
            "i_entities_location_cell",
        ),
        (
            "geo within",
            format!(
                "SELECT entity FROM entities WHERE {w} AND {} ORDER BY id LIMIT 10000",
                geo_where(Rel::Within, "Polygon", &polygon)
            ),
            "i_entities_location_cell",
        ),
        (
            // a rare type (1 %): for a common one an id-ordered read of the
            // primary key stops after a page's worth of rows, which is the
            // right plan and no walk
            "temporal type",
            format!(
                "SELECT m.meta FROM temporal_entities m WHERE m.tenant_id = '{RLS_A}' \
                 AND (m.types[1] = ANY(ARRAY['https://x/M']) OR (cardinality(m.types) > 1 \
                 AND m.types && ARRAY['https://x/M'])) ORDER BY m.id LIMIT 10000"
            ),
            "i_temporal_entities_multi_type",
        ),
        (
            "temporal lastN",
            format!(
                "SELECT data FROM attr_instances WHERE tenant_id = '{RLS_A}' \
                 AND entity_id = 'urn:ngsi-ld:T1:00000001' AND attr_id = 'https://x/speed' \
                 ORDER BY observed_at DESC LIMIT 5"
            ),
            // the lookup btree, under its partition's own name
            "Index Cond: ((tenant_id = 'pgexplain-rls-a'::text) AND (entity_id =",
        ),
        (
            "temporal between",
            format!(
                "SELECT data FROM attr_instances WHERE tenant_id = '{RLS_A}' \
                 AND entity_id = 'urn:ngsi-ld:T1:00000001' AND attr_id = 'https://x/speed' \
                 AND observed_at >= now() - interval '20 minutes' AND observed_at < now() \
                 ORDER BY observed_at"
            ),
            "Index Cond: ((tenant_id = 'pgexplain-rls-a'::text) AND (entity_id =",
        ),
    ];
    let mut bad = Vec::new();
    for (shape, sql, index) in &shapes {
        let plan = plan_of(&pool, RLS_A, sql).await;
        let id_ok = *shape != "id" || plan.contains("id = ANY");
        if !plan.contains(index) || !id_ok || (*shape != "id" && walks(&plan)) {
            bad.push(format!("{shape} (wants {index}):\n{plan}"));
        }
    }
    assert!(
        bad.is_empty(),
        "shapes that walk the Tenant under RLS:\n\n{}",
        bad.join("\n\n")
    );
}

/// T-4907: a deep page skips its OFFSET over ids read from the type index
/// and fetches only the page's documents by primary key; a keyset page
/// (the cursor behind rel=next) seeks the type index past its last id.
/// Both as antares_app under RLS, the statements `store::pg::entity::query_sql`
/// builds (its unit tests pin the text).
#[tokio::test]
async fn deep_and_keyset_pages_read_ids_from_the_type_index() {
    let url = require_db!();
    let pool = pg::connect(&url, 5).await.expect("connect");
    seed_rls(&pool).await;
    let w = format!("tenant_id = '{RLS_A}' AND (expires_at IS NULL OR expires_at > now())");
    let t1 = "'https://x/T1'";
    let arms = [
        format!("{w} AND types[1] = {t1}"),
        format!(
            "{w} AND cardinality(types) > 1 AND types[1] <> ALL(ARRAY[{t1}]) \
             AND (types @> ARRAY[{t1}])"
        ),
    ];
    let ids = arms
        .iter()
        .map(|a| format!("(SELECT id FROM entities WHERE {a} ORDER BY id LIMIT 5101)"))
        .collect::<Vec<_>>()
        .join(" UNION ALL ");
    let deep = format!(
        "SELECT entity AS entity FROM entities WHERE tenant_id = '{RLS_A}' AND id IN (\
         SELECT id FROM ({ids}) AS arms ORDER BY id LIMIT 101 OFFSET 5000) ORDER BY id"
    );
    let plan = plan_of(&pool, RLS_A, &deep).await;
    assert!(
        !plan.contains("Seq Scan") && plan.contains("i_entities_first_type_id"),
        "deep page skips over the type index:\n{plan}"
    );
    assert!(
        plan.contains("using entities_pkey") && plan.contains("(id = "),
        "the page's documents come by primary key lookup:\n{plan}"
    );
    let keyset = type_page(
        &format!("{w} AND id > 'urn:ngsi-ld:T1:00050001'"),
        &[&["https://x/T1"]],
        101,
    );
    let plan = plan_of(&pool, RLS_A, &keyset).await;
    assert!(
        !plan.contains("Seq Scan")
            && plan
                .lines()
                .any(|l| l.contains("Index Cond") && l.contains("(id > ")),
        "a keyset page seeks past its last id:\n{plan}"
    );
}

/// T-4852 Security: the new arms and the geohash cover isolate Tenants as
/// the policy does. Both Tenants hold the same ids at the same places; the
/// broker's role, set to Tenant A, gets only A's rows for a type list, a
/// type AND-group, a geoquery and a temporal type query.
#[tokio::test]
async fn the_new_index_paths_read_one_tenant() {
    use antares_jsonld::core_context;
    use antares_model::TenantId;
    use antares_sql::compile::geo::{GeoSpec, Rel};
    use antares_sql::store::pg::entity::{EntityFilter, PgEntityStore};
    use antares_sql::store::pg::temporal::PgTemporalStore;
    let url = require_db!();
    let admin = pg::connect(&url, 5).await.expect("connect");
    seed_rls(&admin).await;
    app_role(&admin).await;
    let app_url = url.replace("antares:antares@", "antares_app:app@");
    assert_ne!(app_url, url, "test URL must embed antares:antares creds");
    let pool = sqlx::postgres::PgPoolOptions::new()
        .max_connections(2)
        .connect(&app_url)
        .await
        .expect("app-role connect");
    let a = TenantId::new(RLS_A).expect("tenant");
    let ctx = core_context();
    let expand = |term: &str| ctx.expand_key(term);
    let only_a = |rows: &[serde_json::Value], what: &str| {
        assert!(
            !rows.is_empty(),
            "{what}: nothing came back, the test proves nothing"
        );
        for r in rows {
            assert_eq!(
                r["https://x/tenant"], RLS_A,
                "{what} read another Tenant's row: {r}"
            );
        }
    };
    let store = PgEntityStore::new(pool.clone());
    let page = || antares_store::filter::Page {
        offset: 0,
        limit: 1000,
        count: true,
    };
    let lists = [
        vec![
            vec!["https://x/T1".to_owned()],
            vec!["https://x/T2".to_owned()],
        ],
        vec![vec!["https://x/T0".to_owned(), "https://x/M".to_owned()]],
    ];
    for groups in &lists {
        let out = store
            .query(
                &a,
                &EntityFilter {
                    types: Some(groups),
                    expand: &expand,
                    page: Some(page()),
                    ..Default::default()
                },
            )
            .await
            .expect("type query");
        only_a(&out.rows, "a type list");
        let want = if groups.len() == 2 {
            ROWS / 5
        } else {
            ROWS / 100
        };
        assert_eq!(out.total, Some(want), "the count reads one Tenant too");
    }
    let coordinates = serde_json::json!([15.0, 45.5]);
    let geo = [GeoSpec {
        rel: Rel::Near {
            max: Some(2000.0),
            min: None,
        },
        geometry: "Point",
        coordinates: &coordinates,
        geoproperty_iri: "",
    }];
    let out = store
        .query(
            &a,
            &EntityFilter {
                geo: &geo,
                expand: &expand,
                ..Default::default()
            },
        )
        .await
        .expect("geo query");
    only_a(&out.rows, "a geoquery");
    let types = ["https://x/T4".to_owned()];
    let out = PgTemporalStore::new(pool.clone())
        .query(
            &a,
            &antares_store::filter::TemporalFilter {
                types: Some(&types),
                timeproperty: "observedAt",
                expand: &expand,
                page: Some(page()),
                ..Default::default()
            },
        )
        .await
        .expect("temporal query");
    only_a(&out.rows, "a temporal type query");
    // paged temporal Entities need an instance: the first 1000 ids hold them
    assert_eq!(out.total, Some(100), "the count reads one Tenant too");
}

/// 4.22 reaping is the one statement that runs on a timer against the whole
/// table rather than one Tenant, under the service escape: it reads the
/// partial index, not the table, since `expires_at` is NULL for almost
/// every row, which is exactly what the index excludes.
#[tokio::test]
async fn the_expiry_sweep_stays_index_shaped() {
    let url = require_db!();
    let pool = pg::connect(&url, 5).await.expect("connect");
    seed_rls(&pool).await;
    let plan = plan_as(
        &pool,
        "",
        true,
        "SELECT id FROM entities WHERE expires_at IS NOT NULL AND expires_at <= now()",
    )
    .await;
    assert!(
        plan.contains("i_entities_expires"),
        "the 4.22 sweep no longer routes through its partial index — plan:\n{plan}"
    );
    assert!(
        !plan.contains("Seq Scan on entities"),
        "the 4.22 sweep degenerated into a full read of entities — plan:\n{plan}"
    );
}

/// 5.12 registration matching must be index-shaped: the whole point of
/// narrowing candidates through `csource_index` is to stop reading every
/// registration of the tenant per federated request. The type dimension has to
/// resolve through `i_csource_index_type` on BOTH arms — the named types and
/// the "unconstrained" `entity_type IS NULL` one, which is a second index probe
/// and not a filter over the table. Rows are seeded (and ANALYZEd) because on
/// an empty table the choice between two `(tenant_id, …)` indexes is a coin
/// flip that proves nothing.
#[tokio::test]
async fn registration_matching_uses_the_csource_index() {
    let url = require_db!();
    let pool = pg::connect(&url, 5).await.expect("connect");
    let seed = async {
        let mut tx = pool.begin().await.expect("tx");
        sqlx::query(
            "DELETE FROM csource_registrations WHERE tenant_id IN ('pgexplain', 'pgexplain-other')",
        )
        .execute(&mut *tx)
        .await
        .expect("clean");
        sqlx::query(
            "INSERT INTO csource_registrations (tenant_id, id, registration)
             SELECT t, 'urn:csr:x'||g, jsonb_build_object('id', 'urn:csr:x'||g)
               FROM (VALUES ('pgexplain', 2000), ('pgexplain-other', 20000)) v(t, n),
                    generate_series(1, n) g",
        )
        .execute(&mut *tx)
        .await
        .expect("seed regs");
        sqlx::query(
            "INSERT INTO csource_index
               (tenant_id, registration_id, entity_type, endpoint, mode, ops)
             SELECT t, 'urn:csr:x'||g, 'T'||(g % 200), 'http://cs:9090', 1, 0
               FROM (VALUES ('pgexplain', 2000), ('pgexplain-other', 20000)) v(t, n),
                    generate_series(1, n) g",
        )
        .execute(&mut *tx)
        .await
        .expect("seed index");
        sqlx::query("ANALYZE csource_index, csource_registrations")
            .execute(&mut *tx)
            .await
            .expect("analyze");
        tx.commit().await.expect("commit");
    };
    seed.await;

    let plan = plan_of(
        &pool,
        "pgexplain",
        r#"SELECT DISTINCT r.id, r.registration
             FROM csource_registrations r
             JOIN csource_index x
               ON x.tenant_id = r.tenant_id AND x.registration_id = r.id
            WHERE r.tenant_id = 'pgexplain'
              AND (x.entity_type IS NULL OR x.entity_type = ANY(ARRAY['T7']))
            ORDER BY r.id LIMIT 10000"#,
    )
    .await;
    assert!(
        plan.contains("i_csource_index_type"),
        "type narrowing no longer routes through its index — plan:\n{plan}"
    );
    assert!(
        !plan.contains("Seq Scan on csource_index"),
        "the candidate set must never be a full read of csource_index — plan:\n{plan}"
    );

    // The id dimension can only narrow by tenant: `idPattern` rows must survive
    // every id query (only the matcher owns the regex), so the disjunct stays a
    // recheck on top of the index — but a full read of the table is still out.
    let plan = plan_of(
        &pool,
        "pgexplain",
        r#"SELECT DISTINCT r.id, r.registration
             FROM csource_registrations r
             JOIN csource_index x
               ON x.tenant_id = r.tenant_id AND x.registration_id = r.id
            WHERE r.tenant_id = 'pgexplain'
              AND (x.entity_id IS NULL OR x.id_pattern IS NOT NULL
                   OR x.entity_id = ANY(ARRAY['urn:e:1']))
            ORDER BY r.id LIMIT 10000"#,
    )
    .await;
    assert!(
        plan.contains("i_csource_index"),
        "id narrowing no longer routes through an index — plan:\n{plan}"
    );
    assert!(
        !plan.contains("Seq Scan on csource_index"),
        "id narrowing degenerated into a full read — plan:\n{plan}"
    );

    sqlx::query(
        "DELETE FROM csource_registrations WHERE tenant_id IN ('pgexplain', 'pgexplain-other')",
    )
    .execute(&pool)
    .await
    .expect("cleanup");
}

/// The entity query must run unprepared. A prepared statement switches to a
/// generic plan after five runs on a connection, and the generic plan cannot
/// see that `@ > 100` is something `jsonb_path_ops` cannot narrow: it ORs one
/// GIN scan per `q` alternative, each returning the whole type, then
/// rechecks and sorts every row. perf-baseline measured that at 100 000
/// Vehicles as 1.6 s per request instead of 1.5 ms, and 503s at 50 clients
/// (T-3735). An unprepared statement is planned with its values every time.
#[tokio::test]
async fn the_entity_query_is_not_left_prepared() {
    use antares_jsonld::core_context;
    use antares_model::TenantId;
    use antares_sql::store::pg::entity::{EntityFilter, PgEntityStore};

    let url = require_db!();
    // one connection, so the statement cache read below is the query's own
    let pool = pg::connect(&url, 1).await.expect("connect");
    let tenant = TenantId::new("pgunprepared").expect("tenant");
    pg::ensure_tenant(&pool, &tenant).await.expect("tenant row");
    let store = PgEntityStore::new(pool.clone());
    let ctx = core_context();
    let expand = |term: &str| ctx.expand_key(term);
    let q = antares_ql::parse_q(r#"speed>100;brand=="Tesla""#).expect("q");
    for _ in 0..6 {
        store
            .query(
                &tenant,
                &EntityFilter {
                    q: Some(&q),
                    expand: &expand,
                    ..Default::default()
                },
            )
            .await
            .expect("query");
    }
    let left: Vec<String> = sqlx::query_scalar(
        "SELECT statement FROM pg_prepared_statements WHERE statement LIKE '%FROM entities WHERE%'",
    )
    .persistent(false)
    .fetch_all(&pool)
    .await
    .expect("prepared statements");
    assert!(
        left.is_empty(),
        "the entity query stayed prepared: {left:?}"
    );
}

/// T-4781: one type page as the broker's own role sees it. Under RLS a
/// predicate that is not leakproof (`types @> …`, `cardinality`) is never an
/// index condition, so the old `types @> ARRAY[T] ORDER BY id LIMIT n` walked
/// `entities_pkey` through the whole Tenant for 20 rows. Both arms of the
/// store's split must reach their index with the type in the index
/// condition, in id order, and the count must read the indexes alone.
/// Shapes copied from `query_sql`/`count_sql` (their unit tests pin the
/// text); run as a non-superuser, since a superuser skips RLS and hides it.
#[tokio::test]
#[ignore = "flaky on dev: T-4852, run 39600026863"]
async fn a_type_page_reads_its_type_through_an_index_under_rls() {
    let url = require_db!();
    let pool = pg::connect(&url, 5).await.expect("connect");
    let mut tx = pool.begin().await.expect("tx");
    sqlx::query("SELECT pg_advisory_xact_lock(4334)")
        .execute(&mut *tx)
        .await
        .expect("advisory lock");
    for stmt in [
        "DO $$ BEGIN CREATE ROLE antares_app LOGIN PASSWORD 'app';
         EXCEPTION WHEN duplicate_object OR unique_violation THEN NULL; END $$",
        "GRANT USAGE ON SCHEMA public TO antares_app",
        "GRANT SELECT, INSERT, UPDATE, DELETE ON ALL TABLES IN SCHEMA public TO antares_app",
        "DELETE FROM entities WHERE tenant_id = 'pgexplain-types'",
        // ids sort by type, as `urn:ngsi-ld:<Type>:…` ids do: B's rows sit
        // behind all of A's in `entities_pkey`
        "INSERT INTO entities (tenant_id, id, entity, types, created_at, modified_at)
         SELECT 'pgexplain-types', 'urn:ngsi-ld:' || t || ':' || lpad(g::text, 6, '0'),
                '{}', ARRAY['https://x/' || t], now(), now()
           FROM (VALUES ('A', 20000), ('B', 2000)) v(t, n), generate_series(1, n) g",
        "INSERT INTO entities (tenant_id, id, entity, types, created_at, modified_at)
         SELECT 'pgexplain-types', 'urn:ngsi-ld:C:' || g, '{}',
                ARRAY['https://x/C', 'https://x/B'], now(), now()
           FROM generate_series(1, 50) g",
    ] {
        sqlx::query(stmt).execute(&mut *tx).await.expect(stmt);
    }
    tx.commit().await.expect("seed");
    // autovacuum keeps the visibility map that index-only scans need; a
    // freshly seeded table has none yet. A snapshot another session holds
    // while VACUUM runs (a parallel test's transaction) keeps the new rows
    // from being all-visible, and the count then plans bitmap heap scans
    // (strict run 39362539144): vacuum again until the map covers the table.
    let mut visible = (0, 0);
    for _ in 0..60 {
        sqlx::query("VACUUM ANALYZE entities")
            .execute(&pool)
            .await
            .expect("vacuum");
        visible = sqlx::query_as::<_, (i32, i32)>(
            "SELECT relallvisible, relpages FROM pg_class WHERE oid = 'entities'::regclass",
        )
        .fetch_one(&pool)
        .await
        .expect("visibility map");
        if visible.0 >= visible.1 {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(500)).await;
    }

    let w = "tenant_id = 'pgexplain-types' AND (expires_at IS NULL OR expires_at > now())";
    let first = format!("{w} AND types[1] = 'https://x/B'");
    let later = format!(
        "{w} AND cardinality(types) > 1 AND types[1] <> ALL(ARRAY['https://x/B']) \
         AND (types @> ARRAY['https://x/B'])"
    );
    let page = format!(
        "SELECT entity AS entity FROM (\
         (SELECT id, entity FROM entities WHERE {first} ORDER BY id LIMIT 41) UNION ALL \
         (SELECT id, entity FROM entities WHERE {later} ORDER BY id LIMIT 41)) AS entities \
         ORDER BY id LIMIT 21 OFFSET 20"
    );
    let count = format!(
        "SELECT (SELECT count(*) FROM entities WHERE {first}) \
         + (SELECT count(*) FROM entities WHERE {later})"
    );
    for (what, sql, scan) in [
        ("page", &page, "Index Scan"),
        ("count", &count, "Index Only Scan"),
    ] {
        let mut tx = pool.begin().await.expect("tx");
        for s in [
            "SET LOCAL enable_seqscan = off",
            "SET LOCAL ROLE antares_app",
            "SELECT set_config('antares.tenant', 'pgexplain-types', true)",
        ] {
            sqlx::query(s).execute(&mut *tx).await.expect(s);
        }
        let plan: Vec<String> =
            sqlx::query_scalar(sqlx::AssertSqlSafe(format!("EXPLAIN (FORMAT TEXT) {sql}")))
                .fetch_all(&mut *tx)
                .await
                .expect("explain");
        let plan = plan.join("\n");
        assert!(
            plan.contains(&format!("{scan} using i_entities_first_type_id"))
                && plan.lines().any(|l| l.contains("Index Cond")
                    && l.contains("types[1]")
                    && l.contains("'https://x/B'"))
                // either partial index of the multi-type Entities: which one
                // depends on how many other Tenants hold (the RLS seed above)
                && (plan.contains(&format!("{scan} using i_entities_multi_type_id"))
                    || plan.contains("Bitmap Index Scan on i_entities_types_multi")),
            "the {what} no longer reads its type through the type indexes \
             ({} of {} pages all-visible) — plan:\n{plan}",
            visible.0,
            visible.1
        );
        assert!(
            !plan.contains("entities_pkey") && !plan.contains("Seq Scan"),
            "the {what} walks the whole Tenant — plan:\n{plan}"
        );
    }
}

/// T-4906: the history write's probes read the weeks that can hold the
/// instance, not every weekly partition of the Entity (dev 2026-10-19: the
/// dedup UPDATE's join read w37..w42 cold, 7-15 s a write). Shapes copied
/// from `insert_rows` in store/pg/temporal.rs; a generic plan, as sqlx's
/// prepared statements get, run as the RLS-bound role.
#[tokio::test]
async fn the_history_write_probes_read_only_the_weeks_that_can_hold_the_instance() {
    let url = require_db!();
    let pool = pg::connect(&url, 5).await.expect("connect");
    if pg::maintenance::detect_temporal_backend(&pool).await
        != Ok(pg::maintenance::TemporalBackend::Partitioned)
    {
        eprintln!("SKIP: attr_instances is not natively partitioned here");
        return;
    }
    // A row of a Tenant no retention names keeps each week from being dropped
    // as emptied by a parallel test.
    for (name, lo, hi) in [
        ("attr_instances_2003w10", "2003-03-03", "2003-03-10"),
        ("attr_instances_2003w11", "2003-03-10", "2003-03-17"),
        ("attr_instances_2003w12", "2003-03-17", "2003-03-24"),
    ] {
        for stmt in [
            format!("CREATE TABLE IF NOT EXISTS {name} PARTITION OF attr_instances FOR VALUES FROM ('{lo}') TO ('{hi}')"),
            format!(
                "INSERT INTO attr_instances (tenant_id, entity_id, attr_id, instance_id, observed_at,
                                             created_at, modified_at, data)
                 VALUES ('pgexplain-weeks', 'urn:v:w', 'a', 'urn:i:{lo}', '{lo}', '{lo}', '{lo}', '{{}}')
                 ON CONFLICT DO NOTHING"
            ),
        ] {
            sqlx::query(sqlx::AssertSqlSafe(stmt.clone()))
                .execute(&pool)
                .await
                .expect(&stmt);
        }
    }
    // The role's grants, under the lock the type-page test takes for its own:
    // two GRANTs at once fail with "tuple concurrently updated".
    let mut tx = pool.begin().await.expect("tx");
    for stmt in [
        "SELECT pg_advisory_xact_lock(4334)",
        "DO $$ BEGIN CREATE ROLE antares_app LOGIN PASSWORD 'app';
         EXCEPTION WHEN duplicate_object OR unique_violation THEN NULL; END $$",
        "GRANT USAGE ON SCHEMA public TO antares_app",
        "GRANT SELECT, INSERT, UPDATE, DELETE ON ALL TABLES IN SCHEMA public TO antares_app",
    ] {
        sqlx::query(stmt).execute(&mut *tx).await.expect(stmt);
    }
    tx.commit().await.expect("grants");
    let by_observed_at = "SELECT 1 FROM jsonb_array_elements($3) AS e WHERE EXISTS (
           SELECT 1 FROM attr_instances x
           WHERE x.tenant_id = $1 AND x.entity_id = $2 AND x.attr_id = e->>'attr_id'
             AND x.observed_at = (e->>'observed_at')::timestamptz
             AND x.observed_at BETWEEN $4::timestamptz AND $5::timestamptz)";
    let newest = "SELECT (SELECT x.instance_id FROM attr_instances x
           WHERE x.tenant_id = $1 AND x.entity_id = $2 AND x.attr_id = e->>'attr_id'
             AND x.observed_at >= date_trunc('week', now()) - interval '1 week'
             AND $4::text IS NOT NULL AND $5::text IS NOT NULL
           ORDER BY x.observed_at DESC LIMIT 1) FROM jsonb_array_elements($3) AS e";
    let args = "('pgexplain-weeks', 'urn:v:w', '[{\"attr_id\": \"a\", \"observed_at\": \"2003-03-12T00:00:00Z\"}]', \
                 '2003-03-12T00:00:00Z', '2003-03-12T00:00:00Z')";
    for (what, sql) in [("observedAt", by_observed_at), ("newest", newest)] {
        let mut tx = pool.begin().await.expect("tx");
        for s in [
            "SET LOCAL plan_cache_mode = force_generic_plan",
            "SET LOCAL ROLE antares_app",
            "SELECT set_config('antares.tenant', 'pgexplain-weeks', true)",
        ] {
            sqlx::query(s).execute(&mut *tx).await.expect(s);
        }
        sqlx::query(sqlx::AssertSqlSafe(format!(
            "PREPARE t4906_probe(text, text, jsonb, text, text) AS {sql}"
        )))
        .execute(&mut *tx)
        .await
        .expect("prepare");
        let plan: Vec<String> = sqlx::query_scalar(sqlx::AssertSqlSafe(format!(
            "EXPLAIN (FORMAT TEXT) EXECUTE t4906_probe{args}"
        )))
        .fetch_all(&mut *tx)
        .await
        .expect("explain");
        let plan = plan.join("\n");
        sqlx::query("DEALLOCATE t4906_probe")
            .execute(&mut *tx)
            .await
            .expect("deallocate");
        tx.rollback().await.expect("rollback");
        let weeks: std::collections::BTreeSet<&str> = plan
            .split(|c: char| !(c.is_alphanumeric() || c == '_'))
            .filter(|w| w.starts_with("attr_instances_2"))
            // the partition's own name, not its index's
            .filter_map(|w| w.get(.."attr_instances_2003w11".len()))
            .collect();
        match what {
            "observedAt" => assert_eq!(
                weeks.into_iter().collect::<Vec<_>>(),
                ["attr_instances_2003w11"],
                "the {what} probe reads one week — plan:\n{plan}"
            ),
            _ => assert!(
                !weeks.iter().any(|w| w.starts_with("attr_instances_2003")),
                "the {what} probe reads past weeks — plan:\n{plan}"
            ),
        }
        assert!(
            !plan.contains("attr_instances_default") || what == "newest",
            "the {what} probe reads DEFAULT — plan:\n{plan}"
        );
    }
}
