#![cfg(feature = "sqlite")]
//! Live SQLite tests for `Model::where_any` / `Model::where_all`.
//! Eloquent `Model::whereAny($cols, $val)->get()` /
//! `Model::whereAll($cols, $val)->get()` parity.

use rustango::sql::{sqlx, Auto, Pool};
use rustango::Model;

#[derive(Model, Debug, Clone)]
#[rustango(table = "mwa_user")]
#[allow(dead_code)]
pub struct User {
    #[rustango(primary_key)]
    pub id: Auto<i64>,
    #[rustango(max_length = 80)]
    pub username: String,
    #[rustango(max_length = 120)]
    pub email: String,
    #[rustango(max_length = 80)]
    pub display_name: String,
}

async fn make_pool() -> Pool {
    let p = sqlx::SqlitePool::connect("sqlite::memory:")
        .await
        .expect("sqlite memory");
    sqlx::query(
        "CREATE TABLE mwa_user (
            id           INTEGER PRIMARY KEY AUTOINCREMENT,
            username     TEXT NOT NULL,
            email        TEXT NOT NULL,
            display_name TEXT NOT NULL
        )",
    )
    .execute(&p)
    .await
    .unwrap();
    Pool::Sqlite(p)
}

async fn seed(pool: &Pool) {
    for (u, e, d) in [
        ("alice", "alice@x.com", "Alice"),
        ("bob", "bob@x.com", "alice"),
        ("alice2", "x@y.com", "Bobby"),
        ("carol", "alice@y.com", "Carol"),
    ] {
        let mut row = User {
            id: Auto::default(),
            username: u.into(),
            email: e.into(),
            display_name: d.into(),
        };
        row.save_pool(pool).await.unwrap();
    }
}

#[tokio::test]
async fn where_any_or_composes_columns() {
    let pool = make_pool().await;
    seed(&pool).await;
    // Rows where username = 'alice' OR display_name = 'alice'.
    let rows = User::where_any(&["username", "display_name"], "alice", &pool)
        .await
        .unwrap();
    let usernames: Vec<&str> = rows.iter().map(|r| r.username.as_str()).collect();
    assert_eq!(usernames.len(), 2);
    assert!(usernames.contains(&"alice"));
    assert!(usernames.contains(&"bob"));
}

#[tokio::test]
async fn where_any_empty_cols_returns_no_rows() {
    let pool = make_pool().await;
    seed(&pool).await;
    let rows: Vec<User> = User::where_any(&[], "alice", &pool).await.unwrap();
    assert!(rows.is_empty());
}

#[tokio::test]
async fn where_all_and_composes_columns() {
    let pool = make_pool().await;
    seed(&pool).await;
    // Rows where username = 'alice' AND display_name = 'alice' → 0 rows
    // (alice has display_name 'Alice', not 'alice').
    let rows = User::where_all(&["username", "display_name"], "alice", &pool)
        .await
        .unwrap();
    assert_eq!(rows.len(), 0);
    // Same column twice — degenerates to the single predicate.
    let rows = User::where_all(&["username", "username"], "alice", &pool)
        .await
        .unwrap();
    assert_eq!(rows.len(), 1);
}

#[tokio::test]
async fn where_all_empty_cols_returns_every_row() {
    let pool = make_pool().await;
    seed(&pool).await;
    let rows = User::where_all(&[], "ignored", &pool).await.unwrap();
    assert_eq!(rows.len(), 4);
}

#[tokio::test]
async fn unknown_column_errors() {
    let pool = make_pool().await;
    seed(&pool).await;
    let err = User::where_any(&["username", "nope"], "alice", &pool)
        .await
        .unwrap_err();
    assert!(err.to_string().contains("nope"));
}
