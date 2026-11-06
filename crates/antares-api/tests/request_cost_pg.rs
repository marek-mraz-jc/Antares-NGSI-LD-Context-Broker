// SPDX-License-Identifier: EUPL-1.2
//! Request cost on the Postgres store (T-6615, docs/src/request-cost.md):
//! the `statements`, `rows` and `db-ms` a read reports are the statements
//! sqlx ran for it, counted here independently from sqlx's own
//! `sqlx::query` events inside the request's span (as
//! read_statement_budget.rs counts them). Its own binary: it sets the
//! global tracing subscriber. Skips loudly without ANTARES_TEST_DATABASE_URL.
#![cfg(feature = "postgres")]
#![allow(clippy::unwrap_used)] // an unwrap here is the assertion

use antares_api::AppState;
use antares_model::TenantId;
use antares_sql::sqlx;
use axum::body::Body;
use axum::http::{HeaderMap, Request, StatusCode};
use serde_json::Value;
use std::sync::{Arc, Mutex};
use tower::ServiceExt;
use tracing::Instrument;
use tracing_subscriber::layer::{Context, SubscriberExt};
use tracing_subscriber::registry::LookupSpan;
use tracing_subscriber::Layer;

const COUNTED: &str = "request_cost_pg";
const DEFAULT: &str = "https://uri.etsi.org/ngsi-ld/default-context/";

/// Statements and returned rows inside the counted span.
#[derive(Clone, Default)]
struct Seen(Arc<Mutex<(u64, u64)>>);

struct Rows(u64);

impl tracing::field::Visit for Rows {
    fn record_u64(&mut self, f: &tracing::field::Field, v: u64) {
        if f.name() == "rows_returned" {
            self.0 = v;
        }
    }
    fn record_debug(&mut self, _: &tracing::field::Field, _: &dyn std::fmt::Debug) {}
}

impl<S: tracing::Subscriber + for<'a> LookupSpan<'a>> Layer<S> for Seen {
    fn on_event(&self, e: &tracing::Event<'_>, ctx: Context<'_, S>) {
        let in_request = ctx
            .event_scope(e)
            .is_some_and(|mut scope| scope.any(|span| span.name() == COUNTED));
        if e.metadata().target() == "sqlx::query" && in_request {
            let mut rows = Rows(0);
            e.record(&mut rows);
            let mut seen = self.0.lock().unwrap();
            seen.0 += 1;
            seen.1 += rows.0;
        }
    }
}

fn cost(h: &HeaderMap) -> Vec<(String, f64)> {
    h.get("jc-request-cost")
        .expect("the cost header")
        .to_str()
        .unwrap()
        .split(", ")
        .map(|m| {
            let (k, v) = m.split_once('=').unwrap();
            (k.to_owned(), v.parse().unwrap())
        })
        .collect()
}

#[tokio::test(flavor = "current_thread")]
async fn statements_and_rows_are_what_the_store_ran() {
    let Ok(url) = std::env::var("ANTARES_TEST_DATABASE_URL") else {
        eprintln!("SKIP: ANTARES_TEST_DATABASE_URL not set");
        return;
    };
    let seen = Seen::default();
    tracing::subscriber::set_global_default(
        tracing_subscriber::registry()
            .with(antares_sql::cost::statement_layer())
            .with(
                seen.clone().with_filter(
                    tracing_subscriber::filter::Targets::new()
                        .with_target("sqlx::query", tracing::Level::TRACE)
                        .with_target(COUNTED, tracing::Level::INFO),
                ),
            ),
    )
    .expect("the only subscriber of this binary");

    let pool = antares_sql::store::pg::connect(&url, 4)
        .await
        .expect("connect+migrate");
    let tenant = format!("rc{}", &uuid::Uuid::new_v4().simple().to_string()[..12]);
    let tid = TenantId::new(&tenant).expect("tenant");
    antares_sql::store::pg::ensure_tenant(&pool, &tid)
        .await
        .expect("tenant row");
    let mut tx = pool.begin().await.unwrap();
    sqlx::query("SELECT set_config('antares.tenant', $1, true)")
        .bind(&tenant)
        .execute(&mut *tx)
        .await
        .unwrap();
    sqlx::query(
        "INSERT INTO entities (tenant_id, id, entity, types, created_at, modified_at)
         SELECT $1, 'urn:ngsi-ld:Car:' || lpad(n::text, 3, '0'),
                jsonb_build_object('id', 'urn:ngsi-ld:Car:' || lpad(n::text, 3, '0'),
                                   'type', jsonb_build_array($2 || 'Car')),
                ARRAY[$2 || 'Car'], now(), now()
           FROM generate_series(1, 30) AS n",
    )
    .bind(&tenant)
    .bind(DEFAULT)
    .execute(&mut *tx)
    .await
    .unwrap();
    tx.commit().await.unwrap();

    let store = Arc::new(antares_sql::store::any::AnyStore::Pg(
        antares_sql::store::any::PgBackend::new(pool),
    ));
    let mut st = AppState::with_store("me".into(), store, "postgres");
    antares_api::wire(&mut st).await;
    st.request_cost = true;

    let get = || {
        Request::get("/ngsi-ld/v1/entities?type=Car&limit=20")
            .header("NGSILD-Tenant", &tenant)
            .body(Body::empty())
            .unwrap()
    };
    // warm the caches, then count one read
    antares_api::router(st.clone())
        .oneshot(get())
        .await
        .unwrap();
    *seen.0.lock().unwrap() = (0, 0);
    let res = antares_api::router(st.clone())
        .oneshot(get())
        .instrument(tracing::info_span!(target: COUNTED, COUNTED))
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::OK);
    let headers = res.headers().clone();
    let body = axum::body::to_bytes(res.into_body(), usize::MAX)
        .await
        .unwrap();
    let page: Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(page.as_array().map(Vec::len), Some(20));

    let (statements, rows) = *seen.0.lock().unwrap();
    let c = cost(&headers);
    let member = |k: &str| c.iter().find(|(n, _)| n == k).map(|(_, v)| *v).unwrap();
    eprintln!(
        "JC-Request-Cost: {:?}\nServer-Timing: {:?}\nsqlx events in the request: {statements} statements, {rows} rows",
        headers.get("jc-request-cost"),
        headers.get("server-timing")
    );
    assert!(statements >= 1);
    assert_eq!(member("statements"), statements as f64);
    assert_eq!(member("rows"), rows as f64);
    assert!(rows >= 20, "the page's 20 rows are among them");
    assert!(member("db-ms") > 0.0);
}
