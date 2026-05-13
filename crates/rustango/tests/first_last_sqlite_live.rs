//! v0.45 — live SQLite coverage for `QuerySet::first_pool`,
//! `last_pool`, `earliest_pool`, `latest_pool`.
//!
//! Builder sugar over `order_by + limit(1) + fetch_pool`. No DB-side
//! ranking logic; this suite proves the ordering semantics match
//! Django:
//!
//! - `first_pool` returns the first row by current ordering (PK ASC
//!   when no `order_by` is set).
//! - `last_pool` flips every ordering direction, then takes the
//!   first row of the reversed sequence (PK DESC when no order set).
//! - `earliest_pool(field)` / `latest_pool(field)` replace any
//!   prior ordering with `field ASC` / `field DESC`.

#![cfg(feature = "sqlite")]

use rustango::sql::{Auto, Pool};
use rustango::Model;

#[derive(Model, Debug, Clone)]
#[rustango(table = "v045_post")]
pub struct V045Post {
    #[rustango(primary_key)]
    pub id: Auto<i64>,
    #[rustango(max_length = 200)]
    pub title: String,
    pub view_count: i64,
}

async fn pool_with_rows() -> Pool {
    let pool = Pool::connect("sqlite::memory:").await.expect("sqlite");
    rustango::sql::raw_execute_pool(
        &pool,
        "CREATE TABLE v045_post (id INTEGER PRIMARY KEY AUTOINCREMENT, \
         title TEXT NOT NULL, view_count INTEGER NOT NULL)",
        vec![],
    )
    .await
    .expect("create table");
    // Insert deliberately out-of-order so we can prove the sort runs.
    for (title, views) in [
        ("Gamma", 30_i64),
        ("Alpha", 10),
        ("Delta", 40),
        ("Beta", 20),
    ] {
        rustango::sql::raw_execute_pool(
            &pool,
            "INSERT INTO v045_post(title, view_count) VALUES (?, ?)",
            vec![
                rustango::core::SqlValue::String(title.to_owned()),
                rustango::core::SqlValue::I64(views),
            ],
        )
        .await
        .unwrap();
    }
    pool
}

#[tokio::test]
async fn first_pool_without_order_by_returns_lowest_pk() {
    let pool = pool_with_rows().await;
    let first = V045Post::objects()
        .first_pool(&pool)
        .await
        .expect("first_pool")
        .expect("at least one row");
    // Insertion order made "Gamma" the first row; PK ASC also picks it.
    assert_eq!(first.title, "Gamma");
    assert_eq!(first.id.get().copied(), Some(1));
}

#[tokio::test]
async fn last_pool_without_order_by_returns_highest_pk() {
    let pool = pool_with_rows().await;
    let last = V045Post::objects()
        .last_pool(&pool)
        .await
        .expect("last_pool")
        .expect("at least one row");
    // Last-inserted was "Beta" with PK=4.
    assert_eq!(last.title, "Beta");
    assert_eq!(last.id.get().copied(), Some(4));
}

#[tokio::test]
async fn last_pool_flips_existing_ordering() {
    let pool = pool_with_rows().await;
    // Order by title ASC → first is "Alpha". `last_pool` flips that
    // to DESC and takes the first → "Gamma".
    let last = V045Post::objects()
        .order_by(&[("title", false)])
        .last_pool(&pool)
        .await
        .expect("last_pool")
        .expect("row");
    assert_eq!(last.title, "Gamma");
}

#[tokio::test]
async fn earliest_pool_sorts_ascending_by_field() {
    let pool = pool_with_rows().await;
    let earliest = V045Post::objects()
        .earliest_pool("view_count", &pool)
        .await
        .expect("earliest_pool")
        .expect("row");
    assert_eq!(earliest.view_count, 10);
    assert_eq!(earliest.title, "Alpha");
}

#[tokio::test]
async fn latest_pool_sorts_descending_by_field() {
    let pool = pool_with_rows().await;
    let latest = V045Post::objects()
        .latest_pool("view_count", &pool)
        .await
        .expect("latest_pool")
        .expect("row");
    assert_eq!(latest.view_count, 40);
    assert_eq!(latest.title, "Delta");
}

#[tokio::test]
async fn earliest_pool_replaces_prior_ordering() {
    let pool = pool_with_rows().await;
    // Caller mis-ordered to view_count DESC; `earliest_pool` should
    // discard that and sort by title ASC instead.
    let earliest = V045Post::objects()
        .order_by(&[("view_count", true)])
        .earliest_pool("title", &pool)
        .await
        .expect("earliest_pool")
        .expect("row");
    assert_eq!(earliest.title, "Alpha");
}

#[tokio::test]
async fn first_pool_on_empty_table_returns_none() {
    let pool = Pool::connect("sqlite::memory:").await.expect("sqlite");
    rustango::sql::raw_execute_pool(
        &pool,
        "CREATE TABLE v045_post (id INTEGER PRIMARY KEY AUTOINCREMENT, \
         title TEXT NOT NULL, view_count INTEGER NOT NULL)",
        vec![],
    )
    .await
    .unwrap();
    let first = V045Post::objects()
        .first_pool(&pool)
        .await
        .expect("first_pool on empty");
    assert!(first.is_none());
}
