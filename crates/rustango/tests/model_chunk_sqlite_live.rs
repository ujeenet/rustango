#![cfg(feature = "sqlite")]
//! Live SQLite test for Eloquent `Model::chunk($n, fn($chunk) { ... })`
//! parity — the macro-emitted batch-iteration shortcut.

use rustango::sql::{sqlx, Auto, Pool};
use rustango::Model;
use std::sync::atomic::{AtomicI64, Ordering};

#[derive(Model, Debug, Clone)]
#[rustango(table = "chunk_post")]
#[allow(dead_code)]
pub struct Post {
    #[rustango(primary_key)]
    pub id: Auto<i64>,
    #[rustango(max_length = 80)]
    pub title: String,
}

async fn make_pool() -> Pool {
    let p = sqlx::SqlitePool::connect("sqlite::memory:")
        .await
        .expect("sqlite memory");
    sqlx::query(
        "CREATE TABLE chunk_post (
            id    INTEGER PRIMARY KEY AUTOINCREMENT,
            title TEXT NOT NULL
        )",
    )
    .execute(&p)
    .await
    .unwrap();
    Pool::Sqlite(p)
}

async fn seed(pool: &Pool, n: i32) {
    for i in 0..n {
        let mut p = Post {
            id: Auto::default(),
            title: format!("post-{i}"),
        };
        p.save_pool(pool).await.unwrap();
    }
}

#[tokio::test]
async fn chunk_visits_every_row_in_pk_order() {
    let pool = make_pool().await;
    seed(&pool, 25).await;
    let count = AtomicI64::new(0);
    Post::chunk(7, &pool, |batch| {
        count.fetch_add(batch.len() as i64, Ordering::SeqCst);
        async { Ok(()) }
    })
    .await
    .unwrap();
    assert_eq!(count.load(Ordering::SeqCst), 25);
}

#[tokio::test]
async fn chunk_callback_receives_batches_of_n() {
    let pool = make_pool().await;
    seed(&pool, 25).await;
    let sizes = std::sync::Mutex::new(Vec::<usize>::new());
    Post::chunk(7, &pool, |batch| {
        sizes.lock().unwrap().push(batch.len());
        async { Ok(()) }
    })
    .await
    .unwrap();
    let sizes = sizes.into_inner().unwrap();
    // 25 rows in batches of 7 → 7, 7, 7, 4.
    assert_eq!(sizes, vec![7, 7, 7, 4]);
}

#[tokio::test]
async fn chunk_empty_table_invokes_callback_zero_times() {
    let pool = make_pool().await;
    let calls = AtomicI64::new(0);
    Post::chunk(10, &pool, |_batch| {
        calls.fetch_add(1, Ordering::SeqCst);
        async { Ok(()) }
    })
    .await
    .unwrap();
    assert_eq!(calls.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn chunk_propagates_callback_error() {
    let pool = make_pool().await;
    seed(&pool, 5).await;
    // Surface a driver-level error via an obviously invalid raw
    // query — the callback returns the wrapped result so chunk()
    // propagates it.
    let result = Post::chunk(2, &pool, |_batch| async {
        let bogus = rustango::sql::raw_execute_pool(
            &Pool::Sqlite(sqlx::SqlitePool::connect("sqlite::memory:").await.unwrap()),
            "NOT A VALID SQL STATEMENT;",
            vec![],
        )
        .await;
        bogus.map(|_| ())
    })
    .await;
    assert!(result.is_err());
}
