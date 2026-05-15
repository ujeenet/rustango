#![cfg(feature = "postgres")]
//! Live PG test for `QuerySet::iterator` chunked streaming (issue #23).
//! Verifies LIMIT/OFFSET chunking against a 1000-row fixture: 5
//! chunks of 200, both whole-chunk and row-by-row iteration paths
//! see every row exactly once in stable order, the iterator
//! exhausts itself when the result runs out, and the `seen` counter
//! tracks correctly. Skips silently when `DATABASE_URL` is unset.

use std::sync::OnceLock;

use rustango::core::Column as _;
use rustango::sql::{sqlx, Auto, Pool};
use rustango::Model;
use tokio::sync::Mutex;

#[derive(Model, Debug, Clone)]
#[rustango(table = "iter_live_row")]
#[allow(dead_code)]
pub struct Row {
    #[rustango(primary_key)]
    pub id: Auto<i64>,
    pub value: i64,
}

fn lock() -> &'static Mutex<()> {
    static M: OnceLock<Mutex<()>> = OnceLock::new();
    M.get_or_init(|| Mutex::new(()))
}

async fn fresh_pool() -> Option<Pool> {
    let url = std::env::var("DATABASE_URL").ok()?;
    let pg = sqlx::PgPool::connect(&url).await.ok()?;
    sqlx::query(r#"DROP TABLE IF EXISTS "iter_live_row" CASCADE"#)
        .execute(&pg)
        .await
        .unwrap();
    sqlx::query(
        r#"
        CREATE TABLE "iter_live_row" (
            id BIGSERIAL PRIMARY KEY,
            value BIGINT NOT NULL
        )
        "#,
    )
    .execute(&pg)
    .await
    .unwrap();
    // 1000-row fixture — value mirrors id-1 (0..999).
    let mut values = String::new();
    for i in 0..1000 {
        if i > 0 {
            values.push_str(", ");
        }
        values.push_str(&format!("({i})"));
    }
    sqlx::query(&format!(
        r#"INSERT INTO "iter_live_row" ("value") VALUES {values}"#
    ))
    .execute(&pg)
    .await
    .unwrap();
    Some(Pool::Postgres(pg))
}

async fn cleanup(pool: &Pool) {
    // Single-variant Pool with only the `postgres` feature on; the
    // pattern is irrefutable but stays configuration-honest in case
    // mysql/sqlite features ever get co-enabled in this binary.
    #[allow(irrefutable_let_patterns)]
    let Pool::Postgres(pg) = pool
    else {
        return;
    };
    sqlx::query(r#"DROP TABLE IF EXISTS "iter_live_row" CASCADE"#)
        .execute(pg)
        .await
        .unwrap();
}

/// 1000-row table, chunk_size = 200. Expect 5 chunks of 200 + an
/// `Ok(None)` terminator. Every row is yielded exactly once, in `id
/// ASC` order.
#[tokio::test]
async fn next_chunk_yields_all_rows_in_order() {
    let _g = lock().lock().await;
    let Some(pool) = fresh_pool().await else {
        return;
    };

    let mut iter = Row::objects()
        .order_by(&[("id", false)])
        .iterator(200)
        .unwrap();

    let mut all_values: Vec<i64> = Vec::new();
    let mut chunks_seen = 0;
    while let Some(chunk) = iter.next_chunk(&pool).await.unwrap() {
        chunks_seen += 1;
        assert!(
            chunk.len() <= 200,
            "no chunk exceeds chunk_size: {}",
            chunk.len()
        );
        for row in chunk {
            all_values.push(row.value);
        }
    }
    assert_eq!(chunks_seen, 5, "1000 / 200 = 5 chunks");
    assert_eq!(all_values.len(), 1000);
    // Verify order — values should be 0, 1, 2, …, 999.
    for (i, v) in all_values.iter().enumerate() {
        assert_eq!(*v, i as i64, "row at position {i} has value {v}");
    }
    assert_eq!(iter.rows_seen(), 1000);
    assert!(iter.is_exhausted());

    cleanup(&pool).await;
}

/// Row-by-row path: 1000 calls to `next_row`, each yielding one row
/// in `id ASC` order. Verifies the internal `VecDeque` buffer
/// refills correctly between chunks.
#[tokio::test]
async fn next_row_yields_every_row_with_internal_buffering() {
    let _g = lock().lock().await;
    let Some(pool) = fresh_pool().await else {
        return;
    };

    let mut iter = Row::objects()
        .order_by(&[("id", false)])
        .iterator(150) // chunk_size doesn't divide 1000 evenly (7 chunks: 6×150 + 100)
        .unwrap();

    let mut count = 0_i64;
    while let Some(row) = iter.next_row(&pool).await.unwrap() {
        assert_eq!(
            row.value, count,
            "row at position {count} has value {}",
            row.value
        );
        count += 1;
    }
    assert_eq!(count, 1000);
    assert_eq!(iter.rows_seen(), 1000);
    assert!(iter.is_exhausted());

    cleanup(&pool).await;
}

/// Mix `next_chunk` and `next_row` on the same iterator — the
/// internal buffer must drain in row order before the next DB
/// fetch, so row order is preserved across the boundary.
#[tokio::test]
async fn mixed_next_row_and_next_chunk_preserve_order() {
    let _g = lock().lock().await;
    let Some(pool) = fresh_pool().await else {
        return;
    };

    let mut iter = Row::objects()
        .order_by(&[("id", false)])
        .iterator(100)
        .unwrap();

    // Pull 50 rows via next_row — fills + half-drains the first
    // chunk (100 rows fetched, 50 remain in buffer).
    let mut values: Vec<i64> = Vec::new();
    for _ in 0..50 {
        values.push(iter.next_row(&pool).await.unwrap().unwrap().value);
    }
    assert_eq!(values, (0..50).collect::<Vec<i64>>());

    // Now switch to next_chunk — it should first drain the 50
    // buffered rows as one chunk, then continue with fresh DB
    // fetches.
    let next = iter.next_chunk(&pool).await.unwrap().unwrap();
    assert_eq!(
        next.len(),
        50,
        "drained-buffer chunk has 50 rows: got {}",
        next.len()
    );
    for (i, row) in next.iter().enumerate() {
        assert_eq!(row.value, (50 + i) as i64);
    }

    // Continue with full-chunk fetches until exhausted.
    let mut full = values.clone();
    full.extend(next.iter().map(|r| r.value));
    while let Some(chunk) = iter.next_chunk(&pool).await.unwrap() {
        full.extend(chunk.iter().map(|r| r.value));
    }
    assert_eq!(full.len(), 1000);
    for (i, v) in full.iter().enumerate() {
        assert_eq!(*v, i as i64, "mixed-path position {i} value {v}");
    }

    cleanup(&pool).await;
}

/// Filter narrows the result — iterator only sees matching rows.
/// `value BETWEEN 100 AND 199` → 100 rows, chunk_size = 30, expect
/// 4 chunks: 30, 30, 30, 10.
#[tokio::test]
async fn iterator_respects_where_clause_and_short_final_chunk() {
    let _g = lock().lock().await;
    let Some(pool) = fresh_pool().await else {
        return;
    };

    let mut iter = Row::objects()
        .where_(Row::value.gte(100_i64))
        .where_(Row::value.lt(200_i64))
        .order_by(&[("value", false)])
        .iterator(30)
        .unwrap();

    let mut chunks: Vec<usize> = Vec::new();
    while let Some(chunk) = iter.next_chunk(&pool).await.unwrap() {
        chunks.push(chunk.len());
    }
    assert_eq!(chunks, vec![30, 30, 30, 10], "got {chunks:?}");
    assert_eq!(iter.rows_seen(), 100);

    cleanup(&pool).await;
}

/// Empty result set — iterator yields one `Ok(None)` immediately.
#[tokio::test]
async fn empty_result_yields_none_first_call() {
    let _g = lock().lock().await;
    let Some(pool) = fresh_pool().await else {
        return;
    };

    let mut iter = Row::objects()
        .where_(Row::value.gte(10_000_i64)) // no row matches
        .iterator(200)
        .unwrap();

    assert!(iter.next_chunk(&pool).await.unwrap().is_none());
    assert!(iter.is_exhausted());
    assert_eq!(iter.rows_seen(), 0);

    // Subsequent calls also return None — no extra DB query.
    assert!(iter.next_chunk(&pool).await.unwrap().is_none());
    assert!(iter.next_row(&pool).await.unwrap().is_none());

    cleanup(&pool).await;
}
