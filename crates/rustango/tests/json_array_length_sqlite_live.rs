#![cfg(feature = "sqlite")]
//! Live SQLite test for `funcs::json_array_length` — issue #826
//! (Eloquent `whereJsonLength` parity).
//!
//! Confirms the function actually returns the array element count
//! when invoked through a `where_raw(ExprCompare)` filter against
//! SQLite's `json_array_length`.

use rustango::core::funcs::json_array_length;
use rustango::core::{Expr, Op, SqlValue, WhereExpr, F};
use rustango::query::QuerySet;
use rustango::sql::{sqlx, Auto, FetcherPool, Pool};
use rustango::Model;

#[derive(Model, Debug, Clone)]
#[rustango(table = "jal_doc")]
#[allow(dead_code)]
pub struct Doc {
    #[rustango(primary_key)]
    pub id: Auto<i64>,
    pub data: serde_json::Value,
}

async fn make_pool() -> Pool {
    let p = sqlx::SqlitePool::connect("sqlite::memory:")
        .await
        .expect("sqlite memory");
    sqlx::query(
        "CREATE TABLE jal_doc (
            id   INTEGER PRIMARY KEY AUTOINCREMENT,
            data TEXT NOT NULL
        )",
    )
    .execute(&p)
    .await
    .unwrap();
    Pool::Sqlite(p)
}

async fn seed(pool: &Pool) {
    for payload in [
        serde_json::json!({ "tags": [] }),              // 0 tags
        serde_json::json!({ "tags": ["a"] }),           // 1 tag
        serde_json::json!({ "tags": ["a", "b"] }),      // 2 tags
        serde_json::json!({ "tags": ["a", "b", "c"] }), // 3 tags
    ] {
        let mut d = Doc {
            id: Auto::default(),
            data: payload,
        };
        d.save_pool(pool).await.unwrap();
    }
}

#[tokio::test]
async fn where_json_length_gt_filters_correctly() {
    let pool = make_pool().await;
    seed(&pool).await;

    // SELECT * WHERE json_array_length(data -> '$.tags') > 1
    // — but JsonArrayLength on the bare column counts the top-level
    // object's keys differently per dialect. We need the path
    // extraction first. Use the JsonPath helper.
    use rustango::core::funcs::json_path;

    // `data` is the whole JSON object; `tags` is the array we care
    // about. `json_path(data, &["tags"], false)` emits the path
    // operator that returns the nested array as JSON, then
    // `json_array_length` counts its elements.
    let q = QuerySet::<Doc>::default().where_raw(WhereExpr::ExprCompare {
        lhs: json_array_length(json_path(F("data"), &["tags"], false)),
        op: Op::Gt,
        rhs: Expr::Literal(SqlValue::I64(1)),
    });

    let rows: Vec<Doc> = q.fetch(&pool).await.unwrap();
    assert_eq!(
        rows.len(),
        2,
        "expected 2 docs with > 1 tag (2-tag + 3-tag), got: {rows:?}"
    );
}

#[tokio::test]
async fn where_json_length_eq_zero_finds_empty_arrays() {
    let pool = make_pool().await;
    seed(&pool).await;

    use rustango::core::funcs::json_path;

    let q = QuerySet::<Doc>::default().where_raw(WhereExpr::ExprCompare {
        lhs: json_array_length(json_path(F("data"), &["tags"], false)),
        op: Op::Eq,
        rhs: Expr::Literal(SqlValue::I64(0)),
    });

    let rows: Vec<Doc> = q.fetch(&pool).await.unwrap();
    assert_eq!(rows.len(), 1, "expected 1 doc with empty tags array");
}
