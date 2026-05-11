//! Live regression for the v0.34 bi-dialect `contenttypes::*_pool`
//! family. Exercises the new `&Pool`-taking helpers against an
//! in-memory SQLite registry — proves a sqlite-only stack can
//! bootstrap + seed `rustango_content_types` without ever touching
//! Postgres.
//!
//! Companion to `contenttypes_live.rs` (which is PG-gated on
//! `DATABASE_URL`). This file is unconditional — sqlite-in-memory has
//! no infra requirements.

#![cfg(feature = "sqlite")]

use rustango::contenttypes::{self, ContentType};
use rustango::sql::{sqlx, Auto, Pool};
use rustango::Model;

/// Unique-table dummy model — sits in the process-global inventory so
/// `ensure_seeded_pool` has at least one row to insert beyond the
/// ContentType row itself (which is excluded).
#[derive(Model, Debug, Clone)]
#[rustango(table = "ct_pool_live_post")]
#[rustango(app = "blog_pool_live")]
#[allow(dead_code)]
pub struct Post {
    #[rustango(primary_key)]
    pub id: Auto<i64>,
    #[rustango(max_length = 200)]
    pub title: String,
}

#[derive(Model, Debug, Clone)]
#[rustango(table = "ct_pool_live_user")]
#[rustango(app = "auth_pool_live")]
#[allow(dead_code)]
pub struct User {
    #[rustango(primary_key)]
    pub id: Auto<i64>,
    #[rustango(max_length = 100)]
    pub username: String,
}

async fn sqlite_pool() -> Pool {
    Pool::Sqlite(
        sqlx::SqlitePool::connect("sqlite::memory:")
            .await
            .expect("sqlite memory pool"),
    )
}

#[tokio::test]
async fn ensure_table_pool_creates_sqlite_table() {
    let pool = sqlite_pool().await;
    contenttypes::ensure_table_pool(&pool)
        .await
        .expect("ensure_table_pool");
    // Idempotent — second call is a no-op.
    contenttypes::ensure_table_pool(&pool)
        .await
        .expect("ensure_table_pool idempotent");

    // Probe that the table actually exists by issuing a SELECT.
    if let Pool::Sqlite(sq) = &pool {
        let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM rustango_content_types")
            .fetch_one(sq)
            .await
            .expect("select count");
        assert_eq!(count, 0, "table should exist and be empty");
    } else {
        panic!("expected sqlite pool");
    }
}

#[tokio::test]
async fn ensure_seeded_pool_inserts_rows_on_sqlite() {
    let pool = sqlite_pool().await;
    let inserted = contenttypes::ensure_seeded_pool(&pool)
        .await
        .expect("ensure_seeded_pool");
    // Inventory is process-global: every Model from every test in
    // this binary is registered. The minimum guarantee is that the
    // two dummy models above made it in.
    assert!(
        inserted >= 2,
        "expected at least 2 inserted (Post + User from this test), got {inserted}"
    );
}

#[tokio::test]
async fn ensure_seeded_pool_is_idempotent_on_sqlite() {
    let pool = sqlite_pool().await;
    let first = contenttypes::ensure_seeded_pool(&pool)
        .await
        .expect("first seed");
    assert!(first >= 1);
    let second = contenttypes::ensure_seeded_pool(&pool)
        .await
        .expect("second seed");
    assert_eq!(second, 0, "re-seed should insert nothing");
}

#[tokio::test]
async fn by_natural_key_pool_finds_seeded_row_on_sqlite() {
    let pool = sqlite_pool().await;
    contenttypes::ensure_seeded_pool(&pool).await.expect("seed");
    let row = ContentType::by_natural_key_pool(&pool, "blog_pool_live", "post")
        .await
        .expect("lookup");
    let row = row.expect("seeded row should exist");
    assert_eq!(row.app_label, "blog_pool_live");
    assert_eq!(row.model_name, "post");
    assert_eq!(row.table, "ct_pool_live_post");
}

#[tokio::test]
async fn by_natural_key_pool_returns_none_for_unknown_key() {
    let pool = sqlite_pool().await;
    contenttypes::ensure_seeded_pool(&pool).await.expect("seed");
    let row = ContentType::by_natural_key_pool(&pool, "nope", "missing")
        .await
        .expect("lookup");
    assert!(row.is_none());
}
