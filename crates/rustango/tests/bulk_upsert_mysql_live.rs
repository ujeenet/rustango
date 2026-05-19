#![cfg(feature = "mysql")]
//! Live MySQL regression for `Model::bulk_upsert_pool` — closes #267 / T1.5.
//!
//! Mirrors `bulk_upsert_sqlite_live.rs`. Reads `MYSQL_TEST_URL`;
//! skips when unset.
//!
//! MySQL's UPSERT syntax: `INSERT ... ON DUPLICATE KEY UPDATE col =
//! VALUES(col)`. The `target` argument is accepted but ignored — MySQL
//! matches on every UNIQUE index automatically.

use std::sync::OnceLock;

use rustango::sql::{sqlx, Auto, Pool};
use rustango::Model;

#[derive(Model, Debug, Clone)]
#[rustango(table = "bulk_upsert_mysql_post")]
#[rustango(app = "bulk_upsert_mysql_live")]
#[allow(dead_code)]
pub struct Post {
    #[rustango(primary_key)]
    pub id: Auto<i64>,
    #[rustango(max_length = 64, unique)]
    pub slug: String,
    #[rustango(max_length = 200)]
    pub title: String,
    pub view_count: i64,
}

fn serial_lock() -> &'static tokio::sync::Mutex<()> {
    static LOCK: OnceLock<tokio::sync::Mutex<()>> = OnceLock::new();
    LOCK.get_or_init(|| tokio::sync::Mutex::new(()))
}

async fn mysql_pool() -> Option<Pool> {
    let url = std::env::var("MYSQL_TEST_URL").ok()?;
    let mp = sqlx::mysql::MySqlPoolOptions::new()
        .max_connections(5)
        .connect(&url)
        .await
        .ok()?;
    sqlx::query("DROP TABLE IF EXISTS `bulk_upsert_mysql_post`")
        .execute(&mp)
        .await
        .ok()?;
    sqlx::query(
        "CREATE TABLE `bulk_upsert_mysql_post` (
            `id`         BIGINT NOT NULL AUTO_INCREMENT PRIMARY KEY,
            `slug`       VARCHAR(64) NOT NULL UNIQUE,
            `title`      VARCHAR(200) NOT NULL,
            `view_count` BIGINT NOT NULL
        )",
    )
    .execute(&mp)
    .await
    .ok()?;
    Some(Pool::Mysql(mp))
}

async fn fetch_one(pool: &Pool, slug: &str) -> (String, i64) {
    let Pool::Mysql(p) = pool else { unreachable!() };
    let row: (String, i64) = sqlx::query_as(
        "SELECT `title`, `view_count` FROM `bulk_upsert_mysql_post` WHERE `slug` = ?",
    )
    .bind(slug)
    .fetch_one(p)
    .await
    .expect("fetch_one");
    row
}

async fn count(pool: &Pool) -> i64 {
    let Pool::Mysql(p) = pool else { unreachable!() };
    let (c,): (i64,) = sqlx::query_as("SELECT COUNT(*) FROM `bulk_upsert_mysql_post`")
        .fetch_one(p)
        .await
        .expect("count");
    c
}

#[tokio::test]
async fn first_call_inserts_then_second_call_updates_listed_columns_only() {
    let _g = serial_lock().lock().await;
    let Some(p) = mysql_pool().await else {
        eprintln!("skipping: MYSQL_TEST_URL not set");
        return;
    };

    Post::bulk_upsert_pool(
        &[Post {
            id: Auto::default(),
            slug: "a".into(),
            title: "Alpha".into(),
            view_count: 10,
        }],
        &["slug"],
        &["title", "view_count"],
        &p,
    )
    .await
    .expect("first upsert");
    assert_eq!(count(&p).await, 1);
    assert_eq!(fetch_one(&p, "a").await, ("Alpha".into(), 10));

    // Second call — title in update_cols, view_count NOT.
    Post::bulk_upsert_pool(
        &[Post {
            id: Auto::default(),
            slug: "a".into(),
            title: "Alpha (revised)".into(),
            view_count: 999,
        }],
        &["slug"],
        &["title"],
        &p,
    )
    .await
    .expect("second upsert");

    assert_eq!(count(&p).await, 1);
    let (title, view_count) = fetch_one(&p, "a").await;
    assert_eq!(title, "Alpha (revised)");
    assert_eq!(
        view_count, 10,
        "view_count is not in update_cols — must stay 10"
    );
}

#[tokio::test]
async fn bulk_insert_or_ignore_skips_conflicts_on_mysql() {
    let _g = serial_lock().lock().await;
    let Some(p) = mysql_pool().await else {
        eprintln!("skipping: MYSQL_TEST_URL not set");
        return;
    };

    Post::bulk_upsert_pool(
        &[Post {
            id: Auto::default(),
            slug: "a".into(),
            title: "Alpha".into(),
            view_count: 10,
        }],
        &["slug"],
        &["title"],
        &p,
    )
    .await
    .expect("seed");

    Post::bulk_insert_or_ignore_pool(
        &[
            Post {
                id: Auto::default(),
                slug: "a".into(),
                title: "OVERWRITTEN".into(),
                view_count: 999,
            },
            Post {
                id: Auto::default(),
                slug: "b".into(),
                title: "Beta".into(),
                view_count: 2,
            },
        ],
        &p,
    )
    .await
    .expect("insert_or_ignore");

    assert_eq!(count(&p).await, 2);
    let (title, _) = fetch_one(&p, "a").await;
    assert_eq!(title, "Alpha", "existing row stays untouched");
}
