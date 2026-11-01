// SPDX-License-Identifier: EUPL-1.2
//! `GET /types/{type}` (5.7.7) on Postgres reads its answer as one aggregate
//! (T-6294). The fold it replaced paged every Entity of the type in id
//! order, an ordered index scan that waits on one random heap read per
//! Entity: 67 742 Trees answered 503 at dev's 10 s gateway cut. The guard is
//! the statements, which cannot flake: the read through a view whose rule
//! names the type sends no keyset page (`ORDER BY`, `LIMIT`), and its answer
//! is the fold's (an expired Attribute and a hidden one left out). A view
//! with a row condition still folds. Its own process: the tracing subscriber
//! that counts statements is process-wide. Skips loudly without
//! `ANTARES_TEST_DATABASE_URL` (container recipe in `antares-sql/tests/pg.rs`).
#![cfg(feature = "postgres")]

use antares_api::policy::{
    Decision, DecisionFuture, Filter, NotifyDecision, Operation, PolicyEngine, Rule, Subject,
};
use antares_api::AppState;
use antares_model::TenantId;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use serde_json::{json, Value};
use std::sync::{Arc, Mutex};
use tower::ServiceExt;
use tracing::Instrument;
use tracing_subscriber::layer::{Context, SubscriberExt};
use tracing_subscriber::registry::LookupSpan;
use tracing_subscriber::Layer;

const COUNTED: &str = "type_info_summary_pg";
const DEFAULT: &str = "https://uri.etsi.org/ngsi-ld/default-context/";

fn iri(term: &str) -> String {
    format!("{DEFAULT}{term}")
}

/// Every statement sqlx sends inside the counted span.
#[derive(Clone, Default)]
struct Statements(Arc<Mutex<Vec<String>>>);

impl Statements {
    fn take(&self) -> Vec<String> {
        std::mem::take(&mut *self.0.lock().unwrap_or_else(|e| e.into_inner()))
    }
}

struct Sql(String, String);

impl tracing::field::Visit for Sql {
    fn record_str(&mut self, f: &tracing::field::Field, v: &str) {
        match f.name() {
            "summary" => self.0 = v.to_owned(),
            "db.statement" => self.1 = v.split_whitespace().collect::<Vec<_>>().join(" "),
            _ => {}
        }
    }
    fn record_debug(&mut self, f: &tracing::field::Field, v: &dyn std::fmt::Debug) {
        self.record_str(f, &format!("{v:?}"));
    }
}

impl<S: tracing::Subscriber + for<'a> LookupSpan<'a>> Layer<S> for Statements {
    fn on_event(&self, e: &tracing::Event<'_>, ctx: Context<'_, S>) {
        let in_request = ctx
            .event_scope(e)
            .is_some_and(|mut scope| scope.any(|span| span.name() == COUNTED));
        if e.metadata().target() == "sqlx::query" && in_request {
            let mut sql = Sql(String::new(), String::new());
            e.record(&mut sql);
            let text = if sql.1.is_empty() { sql.0 } else { sql.1 };
            self.0.lock().unwrap_or_else(|e| e.into_inner()).push(text);
        }
    }
}

struct Narrowing(Filter);

impl PolicyEngine for Narrowing {
    fn name(&self) -> &str {
        "type-info-summary-pg"
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

/// Shows Trees with `height`, `gone` and `species`, never `secret`.
fn rule(q: Option<&str>) -> Filter {
    Filter {
        rules: vec![Rule {
            types: vec![iri("Tree")],
            q: q.map(|q| Box::new(antares_ql::parse_q(q).expect("q"))),
            attributes: vec![iri("height"), iri("gone"), iri("species")],
            ..Rule::default()
        }],
        ..Filter::default()
    }
}

async fn get(st: &AppState, tenant: &str) -> (StatusCode, Value) {
    let req = Request::builder()
        .uri("/ngsi-ld/v1/types/Tree")
        .header("NGSILD-Tenant", tenant)
        .body(Body::empty())
        .expect("request");
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
    )
}

/// The answer and the statements of one warm read.
async fn counted(st: &AppState, log: &Statements, tenant: &str) -> (Value, Vec<String>) {
    let (code, body) = get(st, tenant).await;
    assert_eq!(code, StatusCode::OK, "warm-up: {body}");
    log.take();
    let (code, body) = get(st, tenant)
        .instrument(tracing::info_span!(target: COUNTED, COUNTED))
        .await;
    assert_eq!(code, StatusCode::OK, "{body}");
    (body, log.take())
}

fn names(body: &Value) -> Vec<String> {
    let mut n: Vec<String> = body["attributeDetails"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|a| a["attributeName"].as_str().map(str::to_owned))
        .collect();
    n.sort();
    n
}

/// 3 000 Trees (every tenth with its only `gone` instance and a second
/// `height` instance expired, every one with `secret`) and 3 000 Lamps; an
/// expired Tree.
#[tokio::test(flavor = "current_thread")]
async fn a_type_info_through_a_type_view_reads_no_keyset_page() {
    let Ok(url) = std::env::var("ANTARES_TEST_DATABASE_URL") else {
        eprintln!("SKIP: ANTARES_TEST_DATABASE_URL not set");
        return;
    };
    let log = Statements::default();
    tracing::subscriber::set_global_default(
        tracing_subscriber::registry().with(
            log.clone().with_filter(
                tracing_subscriber::filter::Targets::new()
                    .with_target("sqlx::query", tracing::Level::TRACE)
                    .with_target(COUNTED, tracing::Level::INFO),
            ),
        ),
    )
    .expect("the only subscriber of this binary");
    let pool = antares_sql::store::pg::connect(&url, 3)
        .await
        .expect("connect");
    let tenant = format!("ts{}", &uuid::Uuid::new_v4().simple().to_string()[..12]);
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
    antares_sql::sqlx::query("SET LOCAL statement_timeout = 0")
        .execute(&mut *tx)
        .await
        .expect("seed timeout");
    // ids out of heap order, as on dev
    antares_sql::sqlx::query(
        "INSERT INTO entities (tenant_id, id, entity, types, created_at, modified_at,
                               expires_at, attrs_expire_at)
         SELECT $1, d.id, jsonb_build_object('id', d.id, 'type', jsonb_build_array($2 || d.t))
                  || CASE WHEN d.t = 'Tree' THEN jsonb_build_object(
                       $2 || 'height', jsonb_build_array(
                         jsonb_build_object('type', 'Property', 'value', d.n))
                         || CASE WHEN d.n % 10 = 0 THEN jsonb_build_array(jsonb_build_object(
                              'type', 'Relationship', 'object', 'urn:ngsi-ld:Lamp:1',
                              'datasetId', 'urn:ngsi-ld:d:1',
                              'expiresAt', '2000-01-01T00:00:00Z')) ELSE '[]'::jsonb END,
                       $2 || 'secret', jsonb_build_array(
                         jsonb_build_object('type', 'Property', 'value', 's')))
                     ELSE jsonb_build_object($2 || 'lit', jsonb_build_array(
                         jsonb_build_object('type', 'Property', 'value', true))) END
                  || CASE WHEN d.t = 'Tree' AND d.n % 10 = 0 THEN jsonb_build_object(
                       $2 || 'gone', jsonb_build_array(jsonb_build_object('type', 'Property',
                         'value', 1, 'expiresAt', '2000-01-01T00:00:00Z')))
                     ELSE '{}'::jsonb END,
                ARRAY[$2 || d.t], now(), now(), NULL::timestamptz,
                CASE WHEN d.t = 'Tree' AND d.n % 10 = 0 THEN '2000-01-01T00:00:00Z'::timestamptz END
           FROM (SELECT t, n, 'urn:ngsi-ld:' || t || ':' || md5(t || n) AS id
                   FROM unnest(ARRAY['Tree','Lamp']) AS t, generate_series(1, 3000) AS n) d
         UNION ALL
         SELECT $1, 'urn:ngsi-ld:Tree:expired',
                jsonb_build_object('id', 'urn:ngsi-ld:Tree:expired',
                  'type', jsonb_build_array($2 || 'Tree'),
                  $2 || 'species', jsonb_build_array(
                    jsonb_build_object('type', 'Property', 'value', 'oak'))),
                ARRAY[$2 || 'Tree'], now(), now(), '2000-01-01T00:00:00Z'::timestamptz, NULL",
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
    let store = Arc::new(antares_sql::store::any::AnyStore::Pg(
        antares_sql::store::any::PgBackend::new(pool.clone()),
    ));

    let mut st = AppState::with_store("me".into(), store.clone(), "postgres")
        .with_policy(Arc::new(Narrowing(rule(None))));
    antares_api::wire(&mut st).await;
    let (body, statements) = counted(&st, &log, &tenant).await;
    assert_eq!(body["entityCount"], json!(3000), "{body}");
    assert_eq!(names(&body), ["height"], "{body}");
    assert_eq!(
        body["attributeDetails"][0]["attributeTypes"],
        json!(["Property"])
    );
    for s in &statements {
        assert!(
            !s.contains("ORDER BY") && !s.contains("LIMIT"),
            "a keyset page of the fold: {s}\nall: {statements:#?}"
        );
    }
    assert!(
        statements.iter().any(|s| s.contains("jsonb_each")),
        "the aggregate: {statements:#?}"
    );

    // a row condition decides per Entity: the fold, with the same answer
    let mut folded = AppState::with_store("me".into(), store, "postgres")
        .with_policy(Arc::new(Narrowing(rule(Some("height>0")))));
    antares_api::wire(&mut folded).await;
    let (fold, statements) = counted(&folded, &log, &tenant).await;
    assert!(
        statements.iter().any(|s| s.contains("ORDER BY")),
        "{statements:#?}"
    );
    assert_eq!(fold["entityCount"], body["entityCount"], "{fold}");
    assert_eq!(fold["attributeDetails"], body["attributeDetails"], "{fold}");
}
