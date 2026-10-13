// SPDX-License-Identifier: EUPL-1.2
//! Retention per Tenant and type on the memory and file arms (`/q/retention`,
//! T-3900), through the driver traits the sweep calls: the same rules
//! `pg_retention.rs` holds the Postgres arm to. Current state of the type,
//! not modified since the cut and without its own `expiresAt`, goes; history
//! of the type before the cut goes; a dry run counts and keeps; the batch
//! bounds each call; another Tenant and another type stay.

use antares_model::TenantId;
use antares_sql::store::any::AnyStore;
use antares_sql::store::{Kind, Store};
use antares_store::{CurrentStateDriver, TemporalDriver};
use serde_json::json;

const V: &str = "https://example.org/Vehicle";
const W: &str = "https://example.org/Stop";
const SPEED: &str = "https://example.org/speed";
const CUT: &str = "2026-01-01T00:05:00Z";

fn tenant(name: &str) -> TenantId {
    TenantId::new(name).expect("tenant")
}

async fn current_state(s: &AnyStore) {
    let (a, b) = (tenant("retaina"), tenant("retainb"));
    for (t, id, ty, modified, expires) in [
        (&a, "urn:v:old", V, "2026-01-01T00:00:00Z", false),
        (&a, "urn:v:fresh", V, "2026-01-01T00:09:00Z", false),
        (&a, "urn:v:own-expiry", V, "2026-01-01T00:00:00Z", true),
        (&a, "urn:w:old", W, "2026-01-01T00:00:00Z", false),
        (&b, "urn:v:old", V, "2026-01-01T00:00:00Z", false),
    ] {
        let mut doc = json!({"id": id, "type": [ty], "modifiedAt": modified});
        if expires {
            doc["expiresAt"] = json!("2099-01-01T00:00:00Z");
        }
        assert!(s.create(t, Kind::Entity, id, doc).await.expect("create"));
    }

    assert_eq!(
        s.retain_current(&a, V, CUT, 1000, true)
            .await
            .expect("store"),
        1
    );
    assert!(
        s.get(&a, Kind::Entity, "urn:v:old")
            .await
            .expect("store")
            .is_some(),
        "a dry run keeps"
    );
    assert_eq!(
        s.retain_current(&a, V, CUT, 1000, false)
            .await
            .expect("store"),
        1
    );
    assert!(s
        .get(&a, Kind::Entity, "urn:v:old")
        .await
        .expect("store")
        .is_none());
    for id in ["urn:v:fresh", "urn:v:own-expiry", "urn:w:old"] {
        assert!(
            s.get(&a, Kind::Entity, id).await.expect("store").is_some(),
            "{id} stays"
        );
    }
    assert!(
        s.get(&b, Kind::Entity, "urn:v:old")
            .await
            .expect("store")
            .is_some(),
        "tenant B untouched"
    );
    assert_eq!(
        s.retain_current(&a, V, CUT, 1000, false)
            .await
            .expect("store"),
        0,
        "idempotent"
    );
    assert_eq!(
        s.retain_current(&tenant("retainnone"), V, CUT, 1000, false)
            .await
            .expect("store"),
        0
    );
}

async fn history(s: &AnyStore) {
    let (h, i) = (tenant("retainh"), tenant("retaini"));
    for t in [&h, &i] {
        for (id, ty) in [("urn:v:1", V), ("urn:w:1", W)] {
            let doc = json!({"id": id, "type": [ty], SPEED: [
                {"type": "Property", "value": 1, "observedAt": "2026-01-01T00:00:00Z"},
                {"type": "Property", "value": 2, "modifiedAt": "2026-01-01T00:09:00Z"}]});
            assert!(s.create(t, Kind::Temporal, id, doc).await.expect("create"));
        }
    }
    let speeds =
        |doc: Option<serde_json::Value>| doc.expect("doc")[SPEED].as_array().map_or(0, Vec::len);

    assert_eq!(
        s.retain_history(&h, V, CUT, 1000, true)
            .await
            .expect("store"),
        1
    );
    assert_eq!(
        speeds(s.get(&h, Kind::Temporal, "urn:v:1").await.expect("store")),
        2,
        "a dry run keeps"
    );
    assert_eq!(
        s.retain_history(&h, V, CUT, 1000, false)
            .await
            .expect("store"),
        1
    );
    assert_eq!(
        speeds(s.get(&h, Kind::Temporal, "urn:v:1").await.expect("store")),
        1
    );
    assert_eq!(
        speeds(s.get(&h, Kind::Temporal, "urn:w:1").await.expect("store")),
        2,
        "the Stop keeps"
    );
    assert_eq!(
        speeds(s.get(&i, Kind::Temporal, "urn:v:1").await.expect("store")),
        2,
        "tenant I untouched"
    );

    // A backlog drains in calls of at most the batch, and an attribute left
    // without instances goes.
    let backlog: Vec<_> = (0..25)
        .map(|n| json!({"type": "Property", "value": n, "observedAt": format!("2025-12-31T23:{n:02}:00Z")}))
        .collect();
    let doc = json!({"id": "urn:v:backlog", "type": V, SPEED: backlog});
    assert!(s
        .create(&h, Kind::Temporal, "urn:v:backlog", doc)
        .await
        .expect("backlog"));
    let mut rounds = Vec::new();
    loop {
        let n = s
            .retain_history(&h, V, CUT, 10, false)
            .await
            .expect("store");
        rounds.push(n);
        if n < 10 {
            break;
        }
    }
    // urn:v:1 has no old instance left, so the backlog's 25 are all there is.
    assert_eq!(rounds, [10, 10, 5]);
    let left = s
        .get(&h, Kind::Temporal, "urn:v:backlog")
        .await
        .expect("store")
        .expect("doc");
    assert!(
        left.get(SPEED).is_none(),
        "an emptied attribute goes: {left}"
    );
    assert_eq!(
        s.retain_history(&tenant("retainnone"), V, CUT, 10, false)
            .await
            .expect("store"),
        0
    );
}

#[tokio::test]
async fn the_memory_store_retains_per_tenant_and_type() {
    let s = AnyStore::Mem(Store::default());
    current_state(&s).await;
    history(&s).await;
}

#[tokio::test]
async fn the_file_store_retains_per_tenant_and_type_and_keeps_it_across_a_reopen() {
    let dir = std::env::temp_dir().join(format!("antares-retention-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("tempdir");
    {
        let s = AnyStore::Mem(Store::open_file(&dir).expect("open"));
        current_state(&s).await;
        history(&s).await;
        s.close().await;
    }
    let s = AnyStore::Mem(Store::open_file(&dir).expect("reopen"));
    let a = tenant("retaina");
    assert!(
        s.get(&a, Kind::Entity, "urn:v:old")
            .await
            .expect("store")
            .is_none(),
        "the drop was persisted"
    );
    assert!(s
        .get(&a, Kind::Entity, "urn:v:fresh")
        .await
        .expect("store")
        .is_some());
    let v1 = s
        .get(&tenant("retainh"), Kind::Temporal, "urn:v:1")
        .await
        .expect("store")
        .expect("doc");
    assert_eq!(
        v1[SPEED].as_array().map(Vec::len),
        Some(1),
        "the trimmed history was persisted"
    );
    drop(s);
    let _ = std::fs::remove_dir_all(&dir);
}
