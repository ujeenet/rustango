//! End-to-end live test for `assert_num_queries` against a real
//! SQLite pool (Django-parity #431). Verifies the per-task counter
//! actually fires from every instrumented `_pool` and `_tx` entry
//! point in [`rustango::sql`].
//!
//! The unit tests (`test_assertions::query_counter::tests`) cover the
//! counter mechanics; this file proves the integration with real SQL
//! execution paths.
//!
//! The header used to claim "every instrumented entry point" while
//! covering writes and two of the seven read paths. What it did not
//! cover was not instrumented, so the file agreed with itself and said
//! nothing. The uncovered ones are now named and asserted below; the
//! PG-only `_on` family is still uncounted and is #1561.

#![cfg(all(feature = "sqlite", feature = "tenancy"))]

use rustango::sql::{sqlx, Auto, CounterPool as _, FetcherPool as _, Pool};
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
        let rows: Vec<Post> = Post::objects().fetch(&pool).await.unwrap();
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

        let rows: Vec<Post> = Post::objects().fetch(&pool).await.unwrap();
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
        let rows: Vec<Post> = Post::objects().fetch(&pool).await.unwrap();
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
        let rows: Vec<Post> = Post::objects().fetch(&pool).await.unwrap();
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
        let _rows: Vec<Post> = Post::objects().fetch(&pool).await.unwrap();
    })
    .await;
}

// =====================================================================
// The read paths the counter could not see
// =====================================================================
//
// The header above claims this file proves the counter fires from
// *every* instrumented `_pool` entry point. It proved it for writes
// (`execute_pool`, one bump for all of them) and for two reads —
// `select_one_row_pool` and `select_rows_pool_with_related`.
//
// Five more read paths issued their query directly and bumped nothing:
// `raw_query_pool`, `select_rows_pool`, `count_rows_pool` (via
// `fetch_scalar_pool`), `fetch_aggregate_pool` and
// `fetch_paginated_pool`. A guard written over any of them counted
// zero and passed — which is the wrong way round for a tool whose only
// job is catching an N+1, since an N+1 is N *reads*.

/// `raw_query_pool` is the raw read every hand-written query in the
/// framework goes through — the media module's listings, tag lookups
/// and sweeps are all built on it.
#[tokio::test]
async fn a_raw_read_is_counted() {
    let pool = fresh_pool().await;
    let mut p = Post {
        id: Auto::default(),
        title: "raw".into(),
    };
    p.insert_pool(&pool).await.unwrap();

    assert_num_queries(1, async {
        let rows: Vec<(i64, String)> = rustango::sql::raw_query_pool(
            "SELECT id, title FROM assert_nq_post",
            Vec::new(),
            &pool,
        )
        .await
        .unwrap();
        assert_eq!(rows.len(), 1);
    })
    .await;
}

/// N raw reads in a loop is the exact shape `assert_num_queries` exists
/// to catch, and it is what `MediaResponse::from_row` was doing per row
/// (#1551 A). Asserting 1 here must fail loudly, not pass at 0.
#[tokio::test]
#[should_panic(expected = "expected 1 queries, observed 4")]
async fn a_raw_read_per_row_is_visible_as_an_n_plus_1() {
    let pool = fresh_pool().await;
    for i in 0..3 {
        let mut p = Post {
            id: Auto::default(),
            title: format!("n-{i}"),
        };
        p.insert_pool(&pool).await.unwrap();
    }

    assert_num_queries(1, async {
        let rows: Vec<(i64,)> =
            rustango::sql::raw_query_pool("SELECT id FROM assert_nq_post", Vec::new(), &pool)
                .await
                .unwrap();
        for (id,) in rows {
            let _: Vec<(String,)> = rustango::sql::raw_query_pool(
                "SELECT title FROM assert_nq_post WHERE id = ?",
                vec![rustango::core::SqlValue::I64(id)],
                &pool,
            )
            .await
            .unwrap();
        }
    })
    .await;
}

/// `count()` compiles to its own statement, so it is its own query —
/// "one count plus one page" is the canonical two-query list view.
#[tokio::test]
async fn a_count_is_counted() {
    let pool = fresh_pool().await;
    let mut p = Post {
        id: Auto::default(),
        title: "c".into(),
    };
    p.insert_pool(&pool).await.unwrap();

    assert_num_queries(2, async {
        assert_eq!(Post::objects().count(&pool).await.unwrap(), 1);
        let rows: Vec<Post> = Post::objects().fetch(&pool).await.unwrap();
        assert_eq!(rows.len(), 1);
    })
    .await;
}

/// A paginated fetch is one statement — `inject_total_count` folds the
/// total into the page query rather than issuing a second one.
#[tokio::test]
async fn a_paginated_fetch_is_counted_once() {
    let pool = fresh_pool().await;
    for i in 0..3 {
        let mut p = Post {
            id: Auto::default(),
            title: format!("p-{i}"),
        };
        p.insert_pool(&pool).await.unwrap();
    }

    assert_num_queries(1, async {
        let page =
            rustango::sql::fetch_paginated_pool::<Post>(Post::objects().limit(2).offset(0), &pool)
                .await
                .unwrap();
        assert_eq!(page.rows.len(), 2);
    })
    .await;
}

/// An aggregate is its own statement — `Model::sum` / `avg` / `min` /
/// `max` and the queryset forms all compile to one and run it through
/// `fetch_aggregate_pool`.
#[tokio::test]
async fn an_aggregate_is_counted() {
    let pool = fresh_pool().await;
    for i in 0..3 {
        let mut p = Post {
            id: Auto::default(),
            title: format!("a-{i}"),
        };
        p.insert_pool(&pool).await.unwrap();
    }

    assert_num_queries(2, async {
        assert_eq!(Post::max::<i64>("id", &pool).await.unwrap(), Some(3));
        assert_eq!(Post::min::<i64>("id", &pool).await.unwrap(), Some(1));
    })
    .await;
}

/// `select_rows_pool` is the plain typed read, without the
/// `select_related` stitching `select_rows_pool_with_related` does.
/// Both are public and both are a query.
#[tokio::test]
async fn a_plain_typed_read_is_counted() {
    let pool = fresh_pool().await;
    let mut p = Post {
        id: Auto::default(),
        title: "plain".into(),
    };
    p.insert_pool(&pool).await.unwrap();

    assert_num_queries(1, async {
        let select = Post::objects().compile().unwrap();
        let rows: Vec<Post> = rustango::sql::select_rows_pool(&pool, &select)
            .await
            .unwrap();
        assert_eq!(rows.len(), 1);
    })
    .await;
}

/// The `_tx` family was half-instrumented: `raw_execute_tx` and
/// `raw_query_tx` bumped, `insert_tx` / `update_tx` / `delete_tx` (all
/// via `execute_tx`) and `select_rows_tx_with_related` did not. A
/// transaction is where a write-heavy handler does most of its work, so
/// that is the half that mattered.
#[tokio::test]
async fn statements_inside_a_transaction_are_counted() {
    let pool = fresh_pool().await;

    assert_num_queries(3, async {
        let mut tx = rustango::sql::transaction_pool(&pool).await.unwrap();
        // 1: raw insert
        rustango::sql::raw_execute_tx(
            &mut tx,
            "INSERT INTO assert_nq_post (title) VALUES ('tx')",
            Vec::new(),
        )
        .await
        .unwrap();
        // 2: raw read back
        let rows: Vec<(i64,)> =
            rustango::sql::raw_query_tx(&mut tx, "SELECT id FROM assert_nq_post", Vec::new())
                .await
                .unwrap();
        assert_eq!(rows.len(), 1);
        // 3: typed read through the tx path that was not counted
        let select = Post::objects().compile().unwrap();
        let typed: Vec<Post> = rustango::sql::select_rows_tx_with_related(&mut tx, &select)
            .await
            .unwrap();
        assert_eq!(typed.len(), 1);
        tx.commit().await.unwrap();
    })
    .await;
}
