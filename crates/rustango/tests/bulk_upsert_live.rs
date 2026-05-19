#![cfg(all(feature = "postgres", feature = "tenancy"))]
//! Live PG regression for `Model::bulk_upsert_pool` — closes #267 / T1.5.
//!
//! Mirrors `bulk_upsert_sqlite_live.rs` + `bulk_upsert_mysql_live.rs`.
//! Reads `DATABASE_URL`; skips when unset.
//!
//! Postgres syntax: `INSERT ... ON CONFLICT (target) DO UPDATE SET col
//! = EXCLUDED.col`. The `target` argument names the column(s) whose
//! uniqueness constraint defines the conflict.

use rustango::sql::{sqlx, Auto, Pool};
use rustango::Model;
use tokio::sync::Mutex;

#[derive(Model, Debug, Clone)]
#[rustango(table = "_bulk_upsert_pg_post")]
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

fn live_lock() -> &'static Mutex<()> {
    use std::sync::OnceLock;
    static M: OnceLock<Mutex<()>> = OnceLock::new();
    M.get_or_init(|| Mutex::new(()))
}

async fn pool() -> Option<Pool> {
    let url = std::env::var("DATABASE_URL").ok()?;
    let pg = sqlx::PgPool::connect(&url).await.ok()?;
    sqlx::query(r#"DROP TABLE IF EXISTS "_bulk_upsert_pg_post" CASCADE"#)
        .execute(&pg)
        .await
        .ok()?;
    sqlx::query(
        r#"CREATE TABLE "_bulk_upsert_pg_post" (
            "id"         BIGSERIAL   PRIMARY KEY,
            "slug"       VARCHAR(64) NOT NULL UNIQUE,
            "title"      VARCHAR(200) NOT NULL,
            "view_count" BIGINT      NOT NULL
        )"#,
    )
    .execute(&pg)
    .await
    .ok()?;
    Some(Pool::Postgres(pg))
}

async fn fetch_one(pool: &Pool, slug: &str) -> (String, i64) {
    let Pool::Postgres(p) = pool else {
        unreachable!()
    };
    let row: (String, i64) = sqlx::query_as(
        r#"SELECT "title", "view_count" FROM "_bulk_upsert_pg_post" WHERE "slug" = $1"#,
    )
    .bind(slug)
    .fetch_one(p)
    .await
    .expect("fetch_one");
    row
}

async fn count(pool: &Pool) -> i64 {
    let Pool::Postgres(p) = pool else {
        unreachable!()
    };
    let (c,): (i64,) = sqlx::query_as(r#"SELECT COUNT(*) FROM "_bulk_upsert_pg_post""#)
        .fetch_one(p)
        .await
        .expect("count");
    c
}

#[tokio::test]
async fn pg_first_call_inserts_then_second_call_updates_listed_only() {
    let _g = live_lock().lock().await;
    let Some(p) = pool().await else {
        eprintln!("skipping: DATABASE_URL not set");
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

    Post::bulk_upsert_pool(
        &[Post {
            id: Auto::default(),
            slug: "a".into(),
            title: "Alpha (revised)".into(),
            view_count: 999,
        }],
        &["slug"],
        &["title"], // view_count NOT in update_cols
        &p,
    )
    .await
    .expect("second upsert");

    let (title, view_count) = fetch_one(&p, "a").await;
    assert_eq!(title, "Alpha (revised)");
    assert_eq!(
        view_count, 10,
        "view_count is not in update_cols — must stay 10"
    );
}

#[tokio::test]
async fn pg_bulk_insert_or_ignore_skips_conflicts() {
    let _g = live_lock().lock().await;
    let Some(p) = pool().await else {
        eprintln!("skipping: DATABASE_URL not set");
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
