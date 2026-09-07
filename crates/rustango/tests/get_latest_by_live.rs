//! Django parity — `Meta.get_latest_by` lets `QuerySet::latest()` /
//! `earliest()` be called without an explicit field arg. rustango wires
//! `#[rustango(get_latest_by = "<col>")]` onto `ModelSchema::get_latest_by`;
//! the new `QuerySet::latest_default(pool)` /
//! `QuerySet::earliest_default(pool)` resolve it at call time.

#![cfg(feature = "sqlite")]

use rustango::sql::{sqlx, Pool};
use rustango::Model;

#[derive(Model, Debug, Clone)]
#[rustango(table = "glb_post", get_latest_by = "created_at")]
#[allow(dead_code)]
pub struct Post {
    #[rustango(primary_key)]
    pub id: i64,
    #[rustango(max_length = 80)]
    pub title: String,
    pub created_at: chrono::DateTime<chrono::Utc>,
}

#[derive(Model, Debug, Clone)]
#[rustango(table = "glb_plain")]
#[allow(dead_code)]
pub struct PlainPost {
    #[rustango(primary_key)]
    pub id: i64,
    pub created_at: chrono::DateTime<chrono::Utc>,
}

async fn fresh_pool() -> Pool {
    let pool = sqlx::SqlitePool::connect("sqlite::memory:")
        .await
        .expect("sqlite pool");
    sqlx::query(
        "CREATE TABLE glb_post (\
            id INTEGER PRIMARY KEY, \
            title TEXT NOT NULL, \
            created_at TEXT NOT NULL)",
    )
    .execute(&pool)
    .await
    .unwrap();
    for (i, (title, ts)) in [
        ("oldest", "2024-01-01T00:00:00+00:00"),
        ("middle", "2025-06-01T00:00:00+00:00"),
        ("newest", "2026-12-31T23:59:59+00:00"),
    ]
    .iter()
    .enumerate()
    {
        sqlx::query("INSERT INTO glb_post (id, title, created_at) VALUES (?, ?, ?)")
            .bind((i + 1) as i64)
            .bind(*title)
            .bind(*ts)
            .execute(&pool)
            .await
            .unwrap();
    }
    Pool::Sqlite(pool)
}

#[test]
fn schema_carries_get_latest_by_tuple() {
    let schema = <Post as rustango::core::Model>::SCHEMA;
    assert_eq!(schema.get_latest_by, Some(("created_at", false)));
    let plain = <PlainPost as rustango::core::Model>::SCHEMA;
    assert_eq!(plain.get_latest_by, None);
}

#[tokio::test]
async fn latest_default_picks_newest_by_meta_field() {
    let pool = fresh_pool().await;
    let row = Post::objects().latest_default(&pool).await.unwrap();
    assert!(row.is_some());
    assert_eq!(row.unwrap().title, "newest");
}

#[tokio::test]
async fn earliest_default_picks_oldest_by_meta_field() {
    let pool = fresh_pool().await;
    let row = Post::objects().earliest_default(&pool).await.unwrap();
    assert!(row.is_some());
    assert_eq!(row.unwrap().title, "oldest");
}

#[tokio::test]
async fn latest_default_errors_without_meta_attr() {
    let pool = fresh_pool().await;
    let err = PlainPost::objects()
        .latest_default(&pool)
        .await
        .expect_err("plain model should error");
    let msg = err.to_string();
    assert!(
        msg.contains("get_latest_by"),
        "error must mention the missing attribute: {msg}"
    );
    assert!(
        msg.contains("PlainPost"),
        "error must name the model: {msg}"
    );
}
