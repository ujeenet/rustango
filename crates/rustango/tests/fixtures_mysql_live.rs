//! v0.41 — MySQL parity for `Fixture::load_into_pool` +
//! `load_all_pool`. Mirrors `fixtures_sqlite_live.rs`.
//!
//! Reads `MYSQL_TEST_URL`. Tests skip silently when unset.

#![cfg(feature = "mysql")]

use std::sync::OnceLock;

use rustango::fixtures::{load_all_pool, Fixture};
use rustango::sql::{sqlx, Pool};
use serde_json::json;

fn serial_lock() -> &'static tokio::sync::Mutex<()> {
    static LOCK: OnceLock<tokio::sync::Mutex<()>> = OnceLock::new();
    LOCK.get_or_init(|| tokio::sync::Mutex::new(()))
}

async fn mysql_pool_with_table() -> Option<Pool> {
    let url = std::env::var("MYSQL_TEST_URL").ok()?;
    let mp = sqlx::mysql::MySqlPoolOptions::new()
        .max_connections(5)
        .connect(&url)
        .await
        .expect("mysql connect");
    let _ = sqlx::query("DROP TABLE IF EXISTS `fixture_widgets`")
        .execute(&mp)
        .await;
    sqlx::query(
        r#"CREATE TABLE `fixture_widgets` (
            `id`     BIGINT NOT NULL PRIMARY KEY,
            `name`   VARCHAR(255) NOT NULL,
            `count`  INT NOT NULL DEFAULT 0,
            `active` TINYINT(1) NOT NULL DEFAULT 0
        )"#,
    )
    .execute(&mp)
    .await
    .expect("create widget table");
    Some(Pool::Mysql(mp))
}

#[tokio::test]
async fn fixture_load_into_pool_inserts_rows_on_mysql() {
    let _serial = serial_lock().lock().await;
    let Some(pool) = mysql_pool_with_table().await else {
        return;
    };

    let f = Fixture::new("widgets")
        .with_row(
            json!({"id": 1, "name": "alpha", "count": 10, "active": true})
                .as_object()
                .unwrap()
                .clone(),
        )
        .with_row(
            json!({"id": 2, "name": "beta", "count": 20, "active": false})
                .as_object()
                .unwrap()
                .clone(),
        );

    let inserted = f
        .load_into_pool("fixture_widgets", &pool)
        .await
        .expect("load_into_pool");
    assert_eq!(inserted, 2);

    if let Pool::Mysql(my) = &pool {
        let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM `fixture_widgets`")
            .fetch_one(my)
            .await
            .expect("count");
        assert_eq!(count, 2);

        let first_name: String =
            sqlx::query_scalar("SELECT `name` FROM `fixture_widgets` WHERE `id` = 1")
                .fetch_one(my)
                .await
                .expect("name");
        assert_eq!(first_name, "alpha");
    }
}

#[tokio::test]
async fn load_all_pool_orders_fixtures_on_mysql() {
    let _serial = serial_lock().lock().await;
    let Some(pool) = mysql_pool_with_table().await else {
        return;
    };

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
