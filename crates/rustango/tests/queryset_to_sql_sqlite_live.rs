#![cfg(feature = "sqlite")]
//! Unit-style test for `QuerySet::to_sql(&pool)` / `to_compiled(&pool)` —
//! Eloquent `Builder::toSql()` parity. No DB round-trip; just verifies
//! the queryset compiles to a SQL string in the pool's dialect.

use rustango::sql::{sqlx, Auto, Pool};
use rustango::Model;

#[derive(Model, Debug, Clone)]
#[rustango(table = "ts_post")]
#[allow(dead_code)]
pub struct Post {
    #[rustango(primary_key)]
    pub id: Auto<i64>,
    #[rustango(max_length = 80)]
    pub title: String,
    pub published: bool,
}

async fn empty_pool() -> Pool {
    let p = sqlx::SqlitePool::connect("sqlite::memory:")
        .await
        .expect("sqlite memory");
    Pool::Sqlite(p)
}

#[tokio::test]
async fn to_sql_renders_select_with_where_and_order_by() {
    let pool = empty_pool().await;
    let sql = Post::objects()
        .filter("published", true)
        .order_by(&[("id", true)])
        .limit(10)
        .to_sql(&pool)
        .unwrap();
    // Sanity: contains the table, the WHERE filter, the ORDER BY, the LIMIT.
    assert!(sql.contains("ts_post"));
    assert!(sql.contains("WHERE"));
    assert!(sql.contains("published"));
    assert!(sql.contains("ORDER BY"));
    assert!(sql.contains("LIMIT"));
}

#[tokio::test]
async fn to_compiled_returns_sql_and_params() {
    let pool = empty_pool().await;
    let stmt = Post::objects()
        .filter("published", true)
        .filter("title", "alpha")
        .to_compiled(&pool)
        .unwrap();
    assert!(stmt.sql.contains("WHERE"));
    // Two filters → two bound parameters.
    assert_eq!(stmt.params.len(), 2);
}

#[tokio::test]
async fn to_sql_errors_on_unknown_field() {
    let pool = empty_pool().await;
    let result = Post::objects().filter("nope", "x").to_sql(&pool);
    assert!(result.is_err(), "unknown field must surface as ExecError");
}
