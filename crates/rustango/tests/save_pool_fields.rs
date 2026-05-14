//! Unit tests for `Model::save_pool_fields` (issue #66) — error paths
//! that don't require a live database. The happy-path "SET clause is
//! actually narrowed" assertion lives in `save_pool_fields_live.rs`
//! against a real PG.

#![cfg(feature = "sqlite")]

use rustango::core::QueryError;
use rustango::sql::{sqlx, Pool};
use rustango::Model;

#[derive(Model, Debug, Clone)]
#[rustango(table = "spf_post")]
#[allow(dead_code)]
pub struct Post {
    #[rustango(primary_key)]
    pub id: i64,
    #[rustango(max_length = 200)]
    pub title: String,
    #[rustango(max_length = 20)]
    pub status: String,
    pub views: i64,
}

async fn fresh_pool() -> Pool {
    let pool = sqlx::SqlitePool::connect("sqlite::memory:")
        .await
        .expect("sqlite pool");
    sqlx::query(
        "CREATE TABLE spf_post (\
            id INTEGER PRIMARY KEY, \
            title TEXT NOT NULL, \
            status TEXT NOT NULL, \
            views INTEGER NOT NULL DEFAULT 0)",
    )
    .execute(&pool)
    .await
    .expect("create table");
    sqlx::query("INSERT INTO spf_post (id, title, status, views) VALUES (1, 'orig', 'draft', 0)")
        .execute(&pool)
        .await
        .expect("seed row");
    Pool::Sqlite(pool)
}

/// Unknown Rust-side field name in the list → `QueryError::UnknownField`.
#[tokio::test]
async fn unknown_field_in_list_errors() {
    let pool = fresh_pool().await;
    let mut row = Post {
        id: 1,
        title: "new".into(),
        status: "published".into(),
        views: 100,
    };
    let r = row.save_pool_fields(&["title", "nope_field"], &pool).await;
    match r {
        Err(rustango::sql::ExecError::Query(QueryError::UnknownField { model, field })) => {
            assert_eq!(model, "Post");
            assert_eq!(field, "nope_field");
        }
        other => panic!("expected ExecError::Query(UnknownField), got {other:?}"),
    }
}

/// Empty field list is a no-op — returns Ok(()) without touching the DB.
#[tokio::test]
async fn empty_field_list_is_a_noop() {
    let pool = fresh_pool().await;
    let mut row = Post {
        id: 1,
        title: "would-overwrite".into(),
        status: "would-overwrite".into(),
        views: 999,
    };
    row.save_pool_fields(&[], &pool)
        .await
        .expect("empty list = no-op");
    // Confirm the original row in the DB is untouched.
    if let Pool::Sqlite(sq) = &pool {
        let (title, status, views): (String, String, i64) =
            sqlx::query_as("SELECT title, status, views FROM spf_post WHERE id = 1")
                .fetch_one(sq)
                .await
                .unwrap();
        assert_eq!(title, "orig");
        assert_eq!(status, "draft");
        assert_eq!(views, 0);
    }
}

/// Happy path on SQLite — only listed columns get written; the others
/// keep their on-disk values even though the in-memory struct mutates
/// them.
#[tokio::test]
async fn only_listed_columns_get_written() {
    let pool = fresh_pool().await;
    let mut row = Post {
        id: 1,
        title: "rewritten".into(),
        // Mutated in memory; should NOT make it to the DB.
        status: "rewritten-status".into(),
        views: 999,
    };
    row.save_pool_fields(&["title"], &pool).await.unwrap();
    if let Pool::Sqlite(sq) = &pool {
        let (title, status, views): (String, String, i64) =
            sqlx::query_as("SELECT title, status, views FROM spf_post WHERE id = 1")
                .fetch_one(sq)
                .await
                .unwrap();
        assert_eq!(title, "rewritten", "title should be updated");
        assert_eq!(
            status, "draft",
            "status should be untouched — not in update_fields list"
        );
        assert_eq!(
            views, 0,
            "views should be untouched — not in update_fields list"
        );
    }
}

/// Multi-field narrowing — two cols listed, one third stays untouched.
#[tokio::test]
async fn multiple_listed_columns_get_written() {
    let pool = fresh_pool().await;
    let mut row = Post {
        id: 1,
        title: "new-title".into(),
        status: "published".into(),
        views: 50,
    };
    row.save_pool_fields(&["title", "views"], &pool)
        .await
        .unwrap();
    if let Pool::Sqlite(sq) = &pool {
        let (title, status, views): (String, String, i64) =
            sqlx::query_as("SELECT title, status, views FROM spf_post WHERE id = 1")
                .fetch_one(sq)
                .await
                .unwrap();
        assert_eq!(title, "new-title");
        assert_eq!(status, "draft", "status not listed → untouched");
        assert_eq!(views, 50);
    }
}

/// Concurrency-safety scenario from the issue rationale: two writers
/// each read the original row, mutate different fields, and call
/// `save_pool_fields` on just their field. The result should reflect
/// BOTH writers' changes — no lost-update.
#[tokio::test]
async fn concurrent_writers_dont_overwrite_each_other() {
    let pool = fresh_pool().await;
    // Writer A: changes title only.
    let mut a = Post {
        id: 1,
        title: "from-A".into(),
        status: "stale-A".into(),
        views: -1,
    };
    a.save_pool_fields(&["title"], &pool).await.unwrap();

    // Writer B: started from the original read (status="draft", views=0),
    // changes status only.
    let mut b = Post {
        id: 1,
        title: "stale-B".into(),
        status: "from-B".into(),
        views: -1,
    };
    b.save_pool_fields(&["status"], &pool).await.unwrap();

    if let Pool::Sqlite(sq) = &pool {
        let (title, status, views): (String, String, i64) =
            sqlx::query_as("SELECT title, status, views FROM spf_post WHERE id = 1")
                .fetch_one(sq)
                .await
                .unwrap();
        assert_eq!(title, "from-A", "A's title write survived");
        assert_eq!(status, "from-B", "B's status write survived");
        assert_eq!(views, 0, "neither writer touched views");
    }
}
