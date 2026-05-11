//! Live regression for v0.35 slice 2 — `ForeignKey::get_pool`
//! against SQLite. Proves FK lazy-load works on any backend without
//! Postgres, mirroring the existing PG-typed `ForeignKey::get` test.
//!
//! Setup: two models (`Author`, `Post`) where `Post.author` is a
//! `ForeignKey<Author>`. Insert one Author, then a Post with an
//! `Unloaded` FK pointing at the Author's PK. Call `get_pool` and
//! assert the parent row is materialized + cached.

#![cfg(feature = "sqlite")]

use rustango::sql::{sqlx, Auto, ForeignKey, Pool};
use rustango::Model;

#[derive(Model, Debug, Clone)]
#[rustango(table = "fk_sqlite_author")]
#[rustango(app = "fk_sqlite_live")]
#[allow(dead_code)]
pub struct Author {
    #[rustango(primary_key)]
    pub id: Auto<i64>,
    #[rustango(max_length = 100)]
    pub name: String,
}

#[derive(Model, Debug, Clone)]
#[rustango(table = "fk_sqlite_post")]
#[rustango(app = "fk_sqlite_live")]
#[allow(dead_code)]
pub struct Post {
    #[rustango(primary_key)]
    pub id: Auto<i64>,
    #[rustango(max_length = 200)]
    pub title: String,
    pub author: ForeignKey<Author>,
}

async fn sqlite_pool_with_schema() -> Pool {
    let pool = sqlx::SqlitePool::connect("sqlite::memory:")
        .await
        .expect("sqlite memory pool");
    sqlx::query(
        r#"CREATE TABLE fk_sqlite_author (
            id    INTEGER PRIMARY KEY AUTOINCREMENT,
            name  TEXT NOT NULL
        )"#,
    )
    .execute(&pool)
    .await
    .expect("create author table");
    // No `REFERENCES` here — we want one test to insert an orphan
    // row and prove `get_pool` raises `ForeignKeyTargetMissing` for
    // the dangling parent (rather than letting sqlite's FK
    // enforcement reject the INSERT before `get_pool` runs).
    sqlx::query(
        r#"CREATE TABLE fk_sqlite_post (
            id     INTEGER PRIMARY KEY AUTOINCREMENT,
            title  TEXT NOT NULL,
            author INTEGER NOT NULL
        )"#,
    )
    .execute(&pool)
    .await
    .expect("create post table");
    Pool::Sqlite(pool)
}

#[tokio::test]
async fn foreign_key_get_pool_resolves_parent_on_sqlite() {
    let pool = sqlite_pool_with_schema().await;

    // Seed an author + a post via the ORM.
    let mut author = Author {
        id: Auto::default(),
        name: "Ada Lovelace".to_owned(),
    };
    author.insert_pool(&pool).await.expect("insert author");
    let author_id = author.id.get().copied().expect("author pk");

    let mut post = Post {
        id: Auto::default(),
        title: "On the Analytical Engine".to_owned(),
        author: ForeignKey::unloaded(author_id),
    };
    post.insert_pool(&pool).await.expect("insert post");

    // FK starts Unloaded.
    assert!(!post.author.is_loaded());

    // get_pool resolves + caches.
    let resolved = post.author.get_pool(&pool).await.expect("get_pool");
    assert_eq!(resolved.name, "Ada Lovelace");
    assert!(post.author.is_loaded());

    // Second call hits the cache (no DB round-trip).
    let cached = post.author.get_pool(&pool).await.expect("get_pool cached");
    assert_eq!(cached.name, "Ada Lovelace");
}

#[tokio::test]
async fn foreign_key_get_pool_errors_on_missing_target() {
    let pool = sqlite_pool_with_schema().await;
    // FK points at a non-existent author. Insert without enforcing FK
    // (sqlite needs `PRAGMA foreign_keys = ON` for that; we leave it
    // off so the bad-FK row goes in and `get_pool` is the one that
    // surfaces the missing parent).
    let mut post = Post {
        id: Auto::default(),
        title: "Orphan".to_owned(),
        author: ForeignKey::unloaded(9999),
    };
    post.insert_pool(&pool).await.expect("insert orphan post");

    let err = post.author.get_pool(&pool).await;
    assert!(err.is_err(), "missing FK target should error");
    let msg = format!("{:?}", err.unwrap_err());
    assert!(
        msg.contains("ForeignKeyTargetMissing") || msg.contains("9999"),
        "error should mention missing FK: {msg}"
    );
}
