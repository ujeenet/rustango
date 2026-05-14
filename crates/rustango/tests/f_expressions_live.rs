#![cfg(feature = "postgres")]
//! Live race-resistance test for `F()` atomic counter updates
//! (issue #1). Without F, the canonical "increment a counter"
//! flow is read-modify-write:
//!
//! ```ignore
//! let post = Post::objects().get(id).await?;
//! post.views += 1;
//! post.save(...).await?;
//! ```
//!
//! …which is a classic lost-update race when two requests fetch the
//! same row in parallel, both increment locally, and both write back.
//! `F()` collapses the read into the UPDATE statement itself:
//!
//! ```ignore
//! Post::objects().filter(...).update().set_expr("views", F("views") + 1).execute_pool(...).await?;
//! ```
//!
//! PostgreSQL takes a row lock for the UPDATE, so 20 concurrent
//! `views = views + 1` calls land on the same row sequentially and
//! the final counter equals the number of calls exactly. This test
//! pins that property end-to-end.
//!
//! Skips silently when `DATABASE_URL` is unset (offline / no service
//! container).

use std::sync::OnceLock;

use rustango::core::{Column as _, F};
use rustango::sql::{sqlx, Auto, Fetcher, Updater};
use rustango::Model;
use tokio::sync::Mutex;

/// Suite-wide lock matching every other PG live test file on this
/// branch — serializes the `DROP TABLE` + `CREATE TABLE` setup so
/// parallel tests don't trip on PG's `pg_class_relname_nsp_index`
/// system-catalog unique.
fn live_lock() -> &'static Mutex<()> {
    static M: OnceLock<Mutex<()>> = OnceLock::new();
    M.get_or_init(|| Mutex::new(()))
}

#[derive(Model, Debug, Clone)]
#[rustango(table = "f_expr_post")]
#[allow(dead_code)]
pub struct Post {
    #[rustango(primary_key)]
    pub id: Auto<i64>,
    #[rustango(max_length = 200)]
    pub title: String,
    pub views: i64,
}

async fn pool() -> Option<sqlx::PgPool> {
    let url = std::env::var("DATABASE_URL").ok()?;
    sqlx::PgPool::connect(&url).await.ok()
}

async fn fresh(pool: &sqlx::PgPool) {
    sqlx::query(r#"DROP TABLE IF EXISTS "f_expr_post" CASCADE"#)
        .execute(pool)
        .await
        .unwrap();
    sqlx::query(
        r#"CREATE TABLE "f_expr_post" (
            "id" BIGSERIAL PRIMARY KEY,
            "title" VARCHAR(200) NOT NULL,
            "views" BIGINT NOT NULL
        )"#,
    )
    .execute(pool)
    .await
    .unwrap();
}

#[tokio::test]
async fn f_expression_increment_is_atomic_under_concurrent_load() {
    let _g = live_lock().lock().await;
    let Some(pool) = pool().await else {
        eprintln!("DATABASE_URL unset — skipping");
        return;
    };
    fresh(&pool).await;

    // Seed one row with views=0.
    let mut row = Post {
        id: Auto::default(),
        title: "race-test".into(),
        views: 0,
    };
    row.insert(&pool).await.unwrap();
    let id = row.id.get().copied().unwrap();

    // Fire N concurrent `UPDATE views = views + 1 WHERE id = ?`.
    // Each goroutine acquires its own PG connection from the pool; the
    // UPDATE itself takes a row-level lock so writes serialize on the
    // server side. Without F() (read-then-write) this would lose at
    // least one increment with high probability.
    const N: usize = 50;
    let mut handles = Vec::with_capacity(N);
    for _ in 0..N {
        let pool = pool.clone();
        handles.push(tokio::spawn(async move {
            Post::objects()
                .eq("id", id)
                .update()
                .set_expr("views", F("views") + 1_i64)
                .execute(&pool)
                .await
                .expect("update succeeds");
        }));
    }
    for h in handles {
        h.await.expect("task didn't panic");
    }

    // Re-fetch and assert the counter equals N — no lost updates.
    let posts: Vec<Post> = Post::objects()
        .where_(Post::id.eq(id))
        .fetch(&pool)
        .await
        .unwrap();
    assert_eq!(posts.len(), 1);
    assert_eq!(
        posts[0].views, N as i64,
        "atomic increment must not lose updates ({} concurrent, final = {})",
        N, posts[0].views
    );

    sqlx::query(r#"DROP TABLE IF EXISTS "f_expr_post" CASCADE"#)
        .execute(&pool)
        .await
        .unwrap();
}

#[tokio::test]
async fn f_expression_column_to_column_compare_filters_correctly() {
    let _g = live_lock().lock().await;
    let Some(pool) = pool().await else {
        return;
    };
    fresh(&pool).await;

    // Seed a few rows where some have views >= title length, some not.
    for (title, views) in [
        ("a", 0_i64), // len 1, views 0  → views < title_len
        ("ab", 10),   // len 2, views 10 → views > title_len
        ("abc", 3),   // len 3, views 3  → views == title_len
        ("abcd", 1),  // len 4, views 1  → views < title_len
    ] {
        let mut p = Post {
            id: Auto::default(),
            title: title.to_owned(),
            views,
        };
        p.insert(&pool).await.unwrap();
    }

    // `views > 2` — sanity literal compare baseline.
    let high: Vec<Post> = Post::objects()
        .where_(Post::views.gt(2_i64))
        .fetch(&pool)
        .await
        .unwrap();
    // Expected matches: "ab" (10) and "abc" (3) = 2 rows.
    assert_eq!(high.len(), 2, "literal compare baseline: {high:?}");

    // `views >= views` — every row matches (trivial column-vs-self).
    // Proves the column-vs-column path emits the right SQL with no params.
    let all: Vec<Post> = Post::objects()
        .where_(Post::views.gte_expr(F("views")))
        .fetch(&pool)
        .await
        .unwrap();
    assert_eq!(all.len(), 4, "self-compare matches every row: {all:?}");

    sqlx::query(r#"DROP TABLE IF EXISTS "f_expr_post" CASCADE"#)
        .execute(&pool)
        .await
        .unwrap();
}
