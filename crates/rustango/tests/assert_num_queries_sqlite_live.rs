//! End-to-end live test for `assert_num_queries` against a real
//! SQLite pool (Django-parity #431). Verifies the per-task counter
//! actually fires from every instrumented `_pool` entry point in
//! [`rustango::sql`].
//!
//! The unit tests (`test_assertions::query_counter::tests`) cover the
//! counter mechanics; this file proves the integration with real SQL
//! execution paths.

#![cfg(all(feature = "sqlite", feature = "tenancy"))]

use rustango::core::{Column as _, Model as _};
use rustango::sql::{sqlx, Auto, FetcherPool as _, Pool};
use rustango::test_assertions::{assert_num_queries, QueryCounter};
use rustango::Model;

#[derive(Model, Debug, Clone)]
#[rustango(table = "assert_nq_post")]
#[rustango(app = "assert_nq_app")]
pub struct Post {
    #[rustango(primary_key)]
    pub id: Auto<i64>,
    #[rustango(max_length = 200)]
    pub title: String,
}

async fn fresh_pool() -> Pool {
    let sq = sqlx::SqlitePool::connect("sqlite::memory:")
        .await
        .expect("sqlite pool");
    sqlx::query(
        "CREATE TABLE assert_nq_post (\
            id INTEGER PRIMARY KEY AUTOINCREMENT, \
            title TEXT NOT NULL)",
    )
    .execute(&sq)
    .await
    .expect("create");
    Pool::Sqlite(sq)
}

#[tokio::test]
async fn assert_num_queries_counts_single_select() {
    let pool = fresh_pool().await;

    // Seed 3 rows OUTSIDE the assert block.
    for i in 0..3 {
        let mut p = Post {
            id: Auto::default(),
            title: format!("seed-{i}"),
        };
        p.insert_pool(&pool).await.unwrap();
    }

    assert_num_queries(1, async {
        let rows: Vec<Post> = Post::objects().fetch_pool(&pool).await.unwrap();
        assert_eq!(rows.len(), 3);
    })
    .await;
}

#[tokio::test]
async fn assert_num_queries_counts_insert_then_select() {
    let pool = fresh_pool().await;

    assert_num_queries(2, async {
        let mut p = Post {
            id: Auto::default(),
            title: "hello".into(),
        };
        p.insert_pool(&pool).await.unwrap();

        let rows: Vec<Post> = Post::objects().fetch_pool(&pool).await.unwrap();
        assert_eq!(rows.len(), 1);
    })
    .await;
}

#[tokio::test]
async fn assert_num_queries_counts_update_delete() {
    let pool = fresh_pool().await;

    // Seed one row outside the block.
    let mut p = Post {
        id: Auto::default(),
        title: "before".into(),
    };
    p.insert_pool(&pool).await.unwrap();
    let id: i64 = *p.id.get().expect("PK assigned");

    assert_num_queries(2, async {
        // 1: UPDATE
        rustango::sql::raw_execute_pool(
            &pool,
            "UPDATE assert_nq_post SET title = 'after' WHERE id = ?",
            vec![rustango::core::SqlValue::I64(id)],
        )
        .await
        .unwrap();
        // 2: DELETE
        rustango::sql::raw_execute_pool(
            &pool,
            "DELETE FROM assert_nq_post WHERE id = ?",
            vec![rustango::core::SqlValue::I64(id)],
        )
        .await
        .unwrap();
    })
    .await;
}

#[tokio::test]
async fn outside_scope_bumps_are_silently_dropped() {
    let pool = fresh_pool().await;

    // Fire 5 queries OUTSIDE any scope — counter doesn't track them.
    for i in 0..5 {
        let mut p = Post {
            id: Auto::default(),
            title: format!("untracked-{i}"),
        };
        p.insert_pool(&pool).await.unwrap();
    }

    // Now open a scope and run exactly 1 query — count must be 1, not 6.
    assert_num_queries(1, async {
        let rows: Vec<Post> = Post::objects().fetch_pool(&pool).await.unwrap();
        assert_eq!(rows.len(), 5);
    })
    .await;
}

#[tokio::test]
async fn scope_take_resets_mid_block() {
    let pool = fresh_pool().await;

    QueryCounter::scope(async {
        // First segment: 2 inserts
        for i in 0..2 {
            let mut p = Post {
                id: Auto::default(),
                title: format!("a-{i}"),
            };
            p.insert_pool(&pool).await.unwrap();
        }
        assert_eq!(QueryCounter::take(), 2);

        // Second segment: 1 select
        let rows: Vec<Post> = Post::objects().fetch_pool(&pool).await.unwrap();
        assert_eq!(rows.len(), 2);
        assert_eq!(QueryCounter::take(), 1);
    })
    .await;
}

#[tokio::test]
#[should_panic(expected = "assertNumQueries failed: expected 1 queries, observed 2")]
async fn fails_loudly_when_count_diverges() {
    let pool = fresh_pool().await;

    assert_num_queries(1, async {
        // Two real queries — should panic.
        let mut p = Post {
            id: Auto::default(),
            title: "x".into(),
        };
        p.insert_pool(&pool).await.unwrap();
        let _rows: Vec<Post> = Post::objects().fetch_pool(&pool).await.unwrap();
    })
    .await;
}
