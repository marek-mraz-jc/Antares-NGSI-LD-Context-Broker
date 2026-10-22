// SPDX-License-Identifier: EUPL-1.2
//! A rollback boots the previous release on the schema a newer release
//! migrated (T-5105, docs/src/operations.md). Needs a live PostGIS; skips when
//! ANTARES_TEST_DATABASE_URL is unset, like tests/pg.rs. It writes rows into
//! `_sqlx_migrations`, which every pg test reads at connect, so it runs in a
//! database of its own: in the shared one a concurrent test's connect met the
//! seeded rows (VersionTooNew(18, 16), train 20261022-0032 fast 39701518856).

#![cfg(feature = "postgres")]

use antares_sql::store::pg;

#[tokio::test(flavor = "multi_thread")]
async fn a_newer_compatible_migration_boots_and_a_breaking_one_stops_the_boot() {
    let Ok(shared) = std::env::var("ANTARES_TEST_DATABASE_URL") else {
        eprintln!("SKIP: ANTARES_TEST_DATABASE_URL not set");
        return;
    };
    let admin = pg::connect(&shared, 2).await.expect("connect+migrate");
    let run = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("clock")
        .as_millis();
    let name = format!("antares_rollback_{run}");
    sqlx::query(sqlx::AssertSqlSafe(format!("CREATE DATABASE {name}")))
        .execute(&admin)
        .await
        .expect("own database");
    let (base, _) = shared.rsplit_once('/').expect("a database URL");
    let url = format!("{base}/{name}");
    let pool = pg::connect(&url, 2)
        .await
        .expect("connect+migrate the own database");
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

/// T-5200: migration 0020 is `compatible`, because it only adds the empty
/// keyed parent and leaves the cut-over NULL. T-5243: 0021 sets the cut-over
/// to a Monday after the migration ran and is `breaking`, because a release
/// before 0020 reads only `attr_instances` and would miss every row observed
/// from it on.
#[tokio::test(flavor = "multi_thread")]
async fn the_cut_over_is_a_monday_after_the_migration_and_breaking() {
    let Ok(url) = std::env::var("ANTARES_TEST_DATABASE_URL") else {
        eprintln!("SKIP: ANTARES_TEST_DATABASE_URL not set");
        return;
    };
    let pool = pg::connect(&url, 2).await.expect("connect+migrate");
    let rows: Vec<(i32, bool)> = sqlx::query_as(
        "SELECT extract(isodow FROM at)::int, at > (SELECT installed_on FROM _sqlx_migrations
                                                     WHERE version = 21)
           FROM history_cutover",
    )
    .fetch_all(&pool)
    .await
    .expect("cut-over");
    assert_eq!(rows, vec![(1, true)], "one row: a Monday after 0021 ran");
    let kind = |v: i64| {
        pg::MIGRATOR
            .iter()
            .find(|m| m.version == v)
            .map(|m| m.description.starts_with("breaking "))
    };
    assert_eq!(kind(20), Some(false), "0020 stays compatible");
    assert_eq!(kind(21), Some(true), "0021 is breaking");
}
