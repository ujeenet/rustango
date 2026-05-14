#![allow(irrefutable_let_patterns, unreachable_patterns)]
// Pool enum is single-variant in sqlite-only builds; patterns become refutable / reachable on multi-backend builds.
//! Live regression for v0.35 slice 3 — `Fixture::load_into_pool` +
//! `load_all_pool` against SQLite. Proves fixture loading works on
//! any backend without Postgres.

#![cfg(feature = "sqlite")]

use rustango::fixtures::{load_all_pool, Fixture};
use rustango::sql::{sqlx, Pool};
use serde_json::json;

async fn sqlite_pool_with_table() -> Pool {
    let pool = sqlx::SqlitePool::connect("sqlite::memory:")
        .await
        .expect("sqlite memory pool");
    sqlx::query(
        r#"CREATE TABLE fixture_widgets (
            id    INTEGER PRIMARY KEY,
            name  TEXT NOT NULL,
            count INTEGER NOT NULL DEFAULT 0,
            active INTEGER NOT NULL DEFAULT 0
        )"#,
    )
    .execute(&pool)
    .await
    .expect("create widget table");
    Pool::Sqlite(pool)
}

#[tokio::test]
async fn fixture_load_into_pool_inserts_rows_on_sqlite() {
    let pool = sqlite_pool_with_table().await;

    let f = Fixture::new("widgets")
        .with_row(
            json!({
                "id": 1, "name": "alpha", "count": 10, "active": true,
            })
            .as_object()
            .unwrap()
            .clone(),
        )
        .with_row(
            json!({
                "id": 2, "name": "beta", "count": 20, "active": false,
            })
            .as_object()
            .unwrap()
            .clone(),
        );

    let inserted = f
        .load_into_pool("fixture_widgets", &pool)
        .await
        .expect("load_into_pool");
    assert_eq!(inserted, 2);

    // Verify the rows landed correctly.
    if let Pool::Sqlite(sq) = &pool {
        let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM fixture_widgets")
            .fetch_one(sq)
            .await
            .expect("count");
        assert_eq!(count, 2);

        let first_name: String =
            sqlx::query_scalar("SELECT name FROM fixture_widgets WHERE id = 1")
                .fetch_one(sq)
                .await
                .expect("name");
        assert_eq!(first_name, "alpha");
    }
}

#[tokio::test]
async fn load_all_pool_orders_fixtures_on_sqlite() {
    let pool = sqlite_pool_with_table().await;
    sqlx::query("DELETE FROM fixture_widgets")
        .execute(match &pool {
            Pool::Sqlite(p) => p,
            _ => unreachable!(),
        })
        .await
        .ok();

    let f1 = Fixture::new("widgets-a").with_row(
        json!({"id": 10, "name": "first", "count": 1, "active": true})
            .as_object()
            .unwrap()
            .clone(),
    );
    let f2 = Fixture::new("widgets-b").with_row(
        json!({"id": 20, "name": "second", "count": 2, "active": false})
            .as_object()
            .unwrap()
            .clone(),
    );

    let total = load_all_pool(&[("fixture_widgets", &f1), ("fixture_widgets", &f2)], &pool)
        .await
        .expect("load_all_pool");
    assert_eq!(total, 2);
}
