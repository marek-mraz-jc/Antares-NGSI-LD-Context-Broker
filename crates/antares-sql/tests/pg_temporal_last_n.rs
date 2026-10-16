// SPDX-License-Identifier: EUPL-1.2
//! lastN read through each Attribute's newest rows (T-4246): the store keeps
//! exactly what ranking the whole window keeps. The oracle here ranks the
//! window's instances by the same canonical text key the SQL rank orders on;
//! the data put offset stamps where the parsed column and that key disagree,
//! and a sparse dataset below the Attribute's N-th newest. Skips loudly
//! without ANTARES_TEST_DATABASE_URL (see tests/pg.rs recipe).

#![cfg(feature = "postgres")]

use std::collections::BTreeSet;

use antares_model::TenantId;
use antares_sql::compile::temporal::InstanceRange;
use antares_sql::store::filter::TemporalFilter;
use antares_sql::store::pg;
use antares_sql::store::pg::temporal::PgTemporalStore;
use serde_json::{json, Value};

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

const SPEED: &str = "https://uri.etsi.org/ngsi-ld/default-context/speed";
const LEVEL: &str = "https://uri.etsi.org/ngsi-ld/default-context/level";

/// `dt_key_sql` for whole-second stamps: a `Z` stamp keys as its first 19
/// characters and a zero fraction, any other as it stands.
fn key(stamp: &str) -> String {
    if stamp.ends_with('Z') && stamp.len() >= 20 {
        format!("{}.000000", &stamp[..19])
    } else {
        stamp.to_owned()
    }
}

fn inst(n: usize, at: &str, dataset: Option<&str>) -> Value {
    let mut i = json!({"type": "Property", "value": n, "observedAt": at,
                       "instanceId": format!("urn:ngsi-ld:Instance:{n}")});
    if let Some(d) = dataset {
        i["datasetId"] = json!(d);
    }
    i
}

/// The instances ranking the window would keep: per datasetId, every
/// instance fewer than `n` others outrank (SQL `rank() <= n`).
fn oracle(instances: &[Value], after: Option<&str>, n: i64) -> BTreeSet<String> {
    let shown: Vec<&Value> = instances
        .iter()
        .filter(|i| after.is_none_or(|a| key(i["observedAt"].as_str().unwrap_or("")) >= key(a)))
        .collect();
    shown
        .iter()
        .filter(|i| {
            let k = key(i["observedAt"].as_str().unwrap_or(""));
            let above = shown
                .iter()
                .filter(|o| o["datasetId"] == i["datasetId"])
                .filter(|o| key(o["observedAt"].as_str().unwrap_or("")) > k)
                .count();
            (above as i64) < n
        })
        .map(|i| i["instanceId"].as_str().unwrap_or("").to_owned())
        .collect()
}

fn ids(rows: &Value) -> BTreeSet<String> {
    rows.as_array()
        .map(|a| {
            a.iter()
                .map(|i| i["instanceId"].as_str().unwrap_or("").to_owned())
                .collect()
        })
        .unwrap_or_default()
}

#[tokio::test(flavor = "multi_thread")]
async fn last_n_keeps_what_ranking_the_whole_window_keeps() {
    let url = require_db!();
    let pool = pg::connect(&url, 5).await.expect("connect");
    let s = PgTemporalStore::new(pool.clone());
    let t = TenantId::new("pglastn").expect("tenant");
    pg::ensure_tenant(&pool, &t).await.expect("tenant row");
    let id = "urn:ngsi-ld:Vehicle:lastn1";
    let _ = s.delete(&t, id).await;

    // speed: 200 hourly Z instances up to 2026-01-09T07:00Z, plus offset
    // stamps the column and the key order differently: +14:00 keys newest
    // and is 14 h older; -12:00 keys older than it is.
    let mut speed: Vec<Value> = (0..200)
        .map(|h| {
            let at = chrono_free(h);
            inst(h, &at, None)
        })
        .collect();
    speed.push(inst(1000, "2026-01-09T20:00:00+14:00", None));
    speed.push(inst(1001, "2026-01-08T20:00:00-12:00", None));
    speed.push(inst(1002, "2026-01-09T07:00:00+00:00", None));
    // level: a busy default and a dataset whose 3 instances are days older
    // than the default's newest, so the Attribute's N-th newest is above them.
    let mut level: Vec<Value> = (0..100)
        .map(|h| inst(2000 + h, &chrono_free(h + 100), None))
        .collect();
    for (j, at) in [
        "2026-01-02T00:00:00Z",
        "2026-01-02T01:00:00Z",
        "2026-01-02T02:00:00Z",
    ]
    .iter()
    .enumerate()
    {
        level.push(inst(3000 + j, at, Some("urn:ngsi-ld:Dataset:sparse")));
    }
    let doc = json!({
        "id": id, "type": "Vehicle",
        "createdAt": "2026-01-01T00:00:00Z", "modifiedAt": "2026-01-01T00:00:00Z",
        SPEED: speed, LEVEL: level,
    });
    assert!(s.create(&t, id, &doc).await.expect("create"));
    // The write records the Attribute with dataset instances (migration 0009),
    // which is what keeps the sparse dataset below from being cut.
    let listed: Vec<String> = sqlx::query_scalar(
        "SELECT unnest(dataset_attrs) FROM temporal_entities WHERE tenant_id = $1 AND id = $2",
    )
    .bind(t.as_str())
    .bind(id)
    .fetch_all(&pool)
    .await
    .expect("dataset_attrs");
    assert_eq!(listed, vec![LEVEL.to_owned()]);

    let expand = |t: &str| t.to_owned();
    for (after, n) in [
        (None, 5),
        (None, 1),
        (Some("2026-01-05T00:00:00Z"), 5),
        (Some("2026-01-09T06:00:00Z"), 3),
        (None, 400),
    ] {
        let range = after.map(|a| InstanceRange {
            timerel: "after",
            time_at: a,
            end_time_at: None,
            timeproperty: "observedAt",
        });
        let f = TemporalFilter {
            ids: Some(&[id]),
            range,
            last_n: Some(n),
            expand: &expand,
            ..TemporalFilter::default()
        };
        let out = s.query(&t, &f).await.expect("query");
        assert_eq!(out.rows.len(), 1, "{after:?} {n}");
        let row = &out.rows[0];
        assert_eq!(
            ids(&row[SPEED]),
            oracle(&doc[SPEED].as_array().expect("speed")[..], after, n),
            "speed {after:?} lastN={n}"
        );
        assert_eq!(
            ids(&row[LEVEL]),
            oracle(&doc[LEVEL].as_array().expect("level")[..], after, n),
            "level {after:?} lastN={n}"
        );
    }
    // the dataset's instances are kept by its own rank, not cut by the default's
    let f = TemporalFilter {
        ids: Some(&[id]),
        last_n: Some(2),
        expand: &expand,
        ..TemporalFilter::default()
    };
    let out = s.query(&t, &f).await.expect("query");
    assert!(
        ids(&out.rows[0][LEVEL]).contains("urn:ngsi-ld:Instance:3002"),
        "{}",
        out.rows[0][LEVEL]
    );

    assert!(s.delete(&t, id).await.expect("delete"));
}

/// `2026-01-01T00:00:00Z` plus `h` hours, without a date library.
fn chrono_free(h: usize) -> String {
    format!("2026-01-{:02}T{:02}:00:00Z", 1 + h / 24, h % 24)
}

/// T-4310: no migration builds an index over the history it would have to read
/// in full. A partial index on `attr_instances` (the first 0009) reads every
/// partition under a lock that blocks INSERT; on dev that was ~145 GB.
#[tokio::test(flavor = "multi_thread")]
async fn no_migration_builds_a_partial_index_over_history() {
    let url = require_db!();
    let pool = pg::connect(&url, 2).await.expect("connect");
    let partial: Vec<String> = sqlx::query_scalar(
        "SELECT c.relname::text FROM pg_index i
         JOIN pg_class c ON c.oid = i.indexrelid
         WHERE i.indrelid = 'attr_instances'::regclass AND i.indpred IS NOT NULL",
    )
    .fetch_all(&pool)
    .await
    .expect("indexes");
    assert!(
        partial.is_empty(),
        "partial indexes on attr_instances: {partial:?}"
    );
}
