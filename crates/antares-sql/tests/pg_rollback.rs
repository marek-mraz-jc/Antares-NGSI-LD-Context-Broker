// SPDX-License-Identifier: EUPL-1.2
//! A rollback boots the previous release on the schema a newer release
//! migrated (T-5105, docs/src/operations.md). Needs a live PostGIS; skips when
//! ANTARES_TEST_DATABASE_URL is unset, like tests/pg.rs. One test in its own
//! binary: it writes rows into `_sqlx_migrations`, which every other pg test
//! reads at connect.

#![cfg(feature = "postgres")]

use antares_sql::store::pg;

#[tokio::test(flavor = "multi_thread")]
async fn a_newer_compatible_migration_boots_and_a_breaking_one_stops_the_boot() {
    let Ok(url) = std::env::var("ANTARES_TEST_DATABASE_URL") else {
        eprintln!("SKIP: ANTARES_TEST_DATABASE_URL not set");
        return;
    };
    let pool = pg::connect(&url, 2).await.expect("connect+migrate");
    let newest = pg::MIGRATOR
        .iter()
        .map(|m| m.version)
        .max()
        .expect("migrations");
    let future = |v: i64, d: &'static str| {
        sqlx::query(
            "INSERT INTO _sqlx_migrations (version, description, success, checksum, execution_time) \
             VALUES ($1, $2, true, '\\x00', 0)",
        )
        .bind(v)
        .bind(d)
    };
    let clean = || sqlx::query("DELETE FROM _sqlx_migrations WHERE version > $1").bind(newest);
    clean().execute(&pool).await.expect("clean");

    future(newest + 1, "type counts")
        .execute(&pool)
        .await
        .expect("seed compatible");
    let compatible = pg::connect(&url, 2)
        .await
        .map(|_| ())
        .map_err(|e| e.to_string());

    future(newest + 2, "breaking drop names")
        .execute(&pool)
        .await
        .expect("seed breaking");
    let breaking = pg::connect(&url, 2)
        .await
        .map(|_| ())
        .map_err(|e| e.to_string());
    let permanent = match pg::connect(&url, 2).await {
        Err(e) => pg::is_schema_mismatch(&e),
        Ok(_) => false,
    };
    clean().execute(&pool).await.expect("clean");

    assert_eq!(compatible, Ok(()));
    assert_eq!(
        breaking,
        Err(format!(
            "migration {} is newer than the latest applied migration {newest}",
            newest + 2
        ))
    );
    assert!(
        permanent,
        "a breaking schema is no reason to retry the boot"
    );
}
