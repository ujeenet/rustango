#![cfg(feature = "sqlite")]
//! Live SQLite regression for `Model::bulk_upsert_pool` — closes
//! #267 / T1.5.
//!
//! Pins the canonical "import a batch, idempotent re-run" pattern
//! that's the top reason users escape to raw_query_pool on every
//! other ORM stack:
//!   1. First call → inserts all rows.
//!   2. Second call with overlapping natural keys → updates listed
//!      columns; non-listed columns stay untouched.
//!   3. ON CONFLICT (slug) DO UPDATE on PG / SQLite; ON DUPLICATE KEY
//!      UPDATE on MySQL. Same model code path on all three.

use rustango::sql::{sqlx, Auto, Pool};
use rustango::Model;

#[derive(Model, Debug, Clone)]
#[rustango(table = "bulk_upsert_post")]
#[rustango(app = "bulk_upsert_sqlite_live")]
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

async fn pool() -> Pool {
    let p = sqlx::SqlitePool::connect("sqlite::memory:")
        .await
        .expect("sqlite memory");
    sqlx::query(
        "CREATE TABLE bulk_upsert_post (
            id         INTEGER PRIMARY KEY AUTOINCREMENT,
            slug       TEXT NOT NULL UNIQUE,
            title      TEXT NOT NULL,
            view_count INTEGER NOT NULL
        )",
    )
    .execute(&p)
    .await
    .unwrap();
    Pool::Sqlite(p)
}

async fn fetch_one(pool: &Pool, slug: &str) -> (String, i64) {
    #[allow(irrefutable_let_patterns)]
    let Pool::Sqlite(p) = pool
    else {
        unreachable!()
    };
    let row: (String, i64) =
        sqlx::query_as(r#"SELECT title, view_count FROM bulk_upsert_post WHERE slug = $1"#)
            .bind(slug)
            .fetch_one(p)
            .await
            .expect("fetch_one");
    row
}

async fn count(pool: &Pool) -> i64 {
    #[allow(irrefutable_let_patterns)]
    let Pool::Sqlite(p) = pool
    else {
        unreachable!()
    };
    let (c,): (i64,) = sqlx::query_as("SELECT COUNT(*) FROM bulk_upsert_post")
        .fetch_one(p)
        .await
        .expect("count");
    c
}

#[tokio::test]
async fn first_call_inserts_all_rows() {
    let p = pool().await;
    let rows = vec![
        Post {
            id: Auto::default(),
            slug: "a".into(),
            title: "Alpha".into(),
            view_count: 1,
        },
        Post {
            id: Auto::default(),
            slug: "b".into(),
            title: "Beta".into(),
            view_count: 2,
        },
    ];
    Post::bulk_upsert_pool(&rows, &["slug"], &["title", "view_count"], &p)
        .await
        .expect("upsert first call");
    assert_eq!(count(&p).await, 2);
    assert_eq!(fetch_one(&p, "a").await, ("Alpha".into(), 1));
}

#[tokio::test]
async fn second_call_updates_listed_columns_only() {
    let p = pool().await;
    let initial = vec![Post {
        id: Auto::default(),
        slug: "a".into(),
        title: "Alpha".into(),
        view_count: 10,
    }];
    Post::bulk_upsert_pool(&initial, &["slug"], &["title", "view_count"], &p)
        .await
        .expect("first upsert");

    // Second call with the same slug — but only `title` in the update
    // list. The view_count column should remain 10 (NOT 999) because
    // it's not in the update_cols set.
    let updated = vec![Post {
        id: Auto::default(),
        slug: "a".into(),
        title: "Alpha (revised)".into(),
        view_count: 999,
    }];
    Post::bulk_upsert_pool(&updated, &["slug"], &["title"], &p)
        .await
        .expect("second upsert");

    // Still exactly one row.
    assert_eq!(count(&p).await, 1);
    let (title, view_count) = fetch_one(&p, "a").await;
    assert_eq!(title, "Alpha (revised)", "title should update");
    assert_eq!(
        view_count, 10,
        "view_count is not in update_cols — must stay 10"
    );
}

#[tokio::test]
async fn bulk_insert_or_ignore_skips_conflicts() {
    let p = pool().await;
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

    // Try to insert the same slug + a new slug — only the new one
    // should land; the existing 'a' row stays untouched.
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
    // 'a' was preserved, not overwritten.
    let (title, view_count) = fetch_one(&p, "a").await;
    assert_eq!(title, "Alpha", "existing row should NOT be overwritten");
    assert_eq!(view_count, 10);
}

#[tokio::test]
async fn empty_batch_is_a_noop() {
    let p = pool().await;
    Post::bulk_upsert_pool(&[], &["slug"], &["title"], &p)
        .await
        .expect("empty upsert");
    Post::bulk_insert_or_ignore_pool(&[], &p)
        .await
        .expect("empty insert_or_ignore");
    assert_eq!(count(&p).await, 0);
}
