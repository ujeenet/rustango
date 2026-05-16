#![cfg(feature = "postgres")]
//! Live PG test for `QuerySet::defer` / `QuerySet::only` (issue #20).
//! Verifies the projection actually skips columns at the wire level —
//! the returned HashMap should contain the kept keys and NOT contain
//! the deferred ones. Skips silently when `DATABASE_URL` is unset.

use std::collections::HashMap;
use std::sync::OnceLock;

use rustango::core::SqlValue;
use rustango::sql::{sqlx, Auto, Pool};
use rustango::Model;
use tokio::sync::Mutex;

#[derive(Model, Debug, Clone)]
#[rustango(table = "do_post_live")]
#[allow(dead_code)]
pub struct Post {
    #[rustango(primary_key)]
    pub id: Auto<i64>,
    #[rustango(max_length = 64)]
    pub title: String,
    /// Pretend this is a multi-kilobyte TEXT column the user wants to
    /// skip on list views.
    pub body: String,
    pub view_count: i64,
}

fn lock() -> &'static Mutex<()> {
    static M: OnceLock<Mutex<()>> = OnceLock::new();
    M.get_or_init(|| Mutex::new(()))
}

async fn fresh_pool() -> Option<Pool> {
    let url = std::env::var("DATABASE_URL").ok()?;
    let pg = sqlx::PgPool::connect(&url).await.ok()?;
    sqlx::query(r#"DROP TABLE IF EXISTS "do_post_live" CASCADE"#)
        .execute(&pg)
        .await
        .unwrap();
    sqlx::query(
        r#"
        CREATE TABLE "do_post_live" (
            id BIGSERIAL PRIMARY KEY,
            title VARCHAR(64) NOT NULL,
            body TEXT NOT NULL,
            view_count BIGINT NOT NULL
        )
        "#,
    )
    .execute(&pg)
    .await
    .unwrap();
    let pool = Pool::Postgres(pg);
    for (title, body, vc) in [
        ("First", "lots of body text 1", 10_i64),
        ("Second", "lots of body text 2", 20),
        ("Third", "lots of body text 3", 30),
    ] {
        let mut p = Post {
            id: Auto::default(),
            title: title.into(),
            body: body.into(),
            view_count: vc,
        };
        p.insert_pool(&pool).await.unwrap();
    }
    Some(pool)
}

/// `.only(&["id", "title"])` returns only id + title — the body
/// column is never touched at the wire level.
#[tokio::test]
async fn only_returns_just_requested_cols() {
    let _g = lock().lock().await;
    let Some(pool) = fresh_pool().await else {
        return;
    };

    let rows: Vec<HashMap<String, SqlValue>> = Post::objects()
        .only(&["id", "title"])
        .fetch(&pool)
        .await
        .unwrap();

    assert_eq!(rows.len(), 3);
    for row in &rows {
        assert_eq!(row.len(), 2, "only id + title: {row:?}");
        assert!(row.contains_key("id"));
        assert!(row.contains_key("title"));
        assert!(!row.contains_key("body"));
        assert!(!row.contains_key("view_count"));
    }
}

/// `.defer(&["body"])` returns every column EXCEPT body — id, title,
/// and view_count survive.
#[tokio::test]
async fn defer_returns_all_cols_except_excluded() {
    let _g = lock().lock().await;
    let Some(pool) = fresh_pool().await else {
        return;
    };

    let rows: Vec<HashMap<String, SqlValue>> =
        Post::objects().defer(&["body"]).fetch(&pool).await.unwrap();

    assert_eq!(rows.len(), 3);
    for row in &rows {
        assert_eq!(row.len(), 3, "id + title + view_count: {row:?}");
        assert!(row.contains_key("id"));
        assert!(row.contains_key("title"));
        assert!(row.contains_key("view_count"));
        assert!(!row.contains_key("body"));
    }
}
