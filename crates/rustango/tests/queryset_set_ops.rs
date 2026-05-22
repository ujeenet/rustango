//! Django-parity #329 — `.union()` / `.intersection()` / `.difference()`
//! exercised against a real (sqlite) DB.
//!
//! The IR + writer paths have been in place since the issue-#25 work;
//! this regression locks in that **all three operators** compile and
//! execute correctly on SQLite. PG / MySQL parity rides on the same
//! [`crate::core::SetOp::keyword`] tokens (UNION / INTERSECT / EXCEPT
//! are SQL-standard); the MySQL caveat (8.0.31+ for INTERSECT/EXCEPT)
//! is operator-visible in the dialect's emitter, not in the IR.

#![cfg(feature = "sqlite")]

use rustango::core::{Model as _, SqlValue};
use rustango::query::QuerySet;
use rustango::sql::Pool;
use rustango::Model;

#[derive(Model, Debug, Clone)]
#[rustango(table = "qsso_post")]
#[allow(dead_code)]
pub struct QssoPost {
    #[rustango(primary_key)]
    id: rustango::Auto<i64>,
    #[rustango(max_length = 200)]
    title: String,
    #[rustango(max_length = 32)]
    status: String,
}

async fn build_pool() -> Pool {
    let pool = Pool::connect("sqlite::memory:").await.expect("sqlite pool");
    rustango::sql::raw_execute_pool(
        &pool,
        r#"CREATE TABLE IF NOT EXISTS "qsso_post" (
            "id"     INTEGER PRIMARY KEY AUTOINCREMENT,
            "title"  TEXT NOT NULL,
            "status" TEXT NOT NULL
        )"#,
        Vec::new(),
    )
    .await
    .expect("create");
    for (title, status) in [
        ("alpha", "draft"),
        ("beta", "draft"),
        ("gamma", "published"),
        ("delta", "published"),
        ("alpha", "archived"),
    ] {
        rustango::sql::raw_execute_pool(
            &pool,
            r#"INSERT INTO "qsso_post" ("title", "status") VALUES (?, ?)"#,
            vec![
                SqlValue::String(title.into()),
                SqlValue::String(status.into()),
            ],
        )
        .await
        .expect("seed");
    }
    pool
}

async fn count_via_select(pool: &Pool, q: rustango::core::SelectQuery) -> usize {
    let scalar: Vec<_> = QssoPost::SCHEMA.scalar_fields().collect();
    rustango::sql::select_rows_as_json(pool, &q, &scalar)
        .await
        .expect("select")
        .len()
}

#[tokio::test]
async fn union_combines_distinct_rows() {
    let pool = build_pool().await;
    let drafts = QuerySet::<QssoPost>::new().filter("status", "draft");
    let published = QuerySet::<QssoPost>::new().filter("status", "published");
    let q = drafts.union(published).compile().unwrap();
    let n = count_via_select(&pool, q).await;
    assert_eq!(n, 4, "union of drafts + published should yield 4 rows");
}

#[tokio::test]
async fn intersection_keeps_only_shared_rows() {
    // alpha appears in both `draft` and `archived` partitions; the
    // intersection of "rows whose title is alpha" with "rows whose
    // status is draft" should leave exactly one row.
    let pool = build_pool().await;
    let alphas = QuerySet::<QssoPost>::new().filter("title", "alpha");
    let drafts = QuerySet::<QssoPost>::new().filter("status", "draft");
    let q = alphas.intersection(drafts).compile().unwrap();
    let n = count_via_select(&pool, q).await;
    assert_eq!(n, 1, "intersection of alpha + draft should yield 1 row");
}

#[tokio::test]
async fn difference_removes_subtracted_rows() {
    // Rows with title=alpha minus rows with status=archived → only
    // the draft alpha survives.
    let pool = build_pool().await;
    let alphas = QuerySet::<QssoPost>::new().filter("title", "alpha");
    let archived = QuerySet::<QssoPost>::new().filter("status", "archived");
    let q = alphas.difference(archived).compile().unwrap();
    let n = count_via_select(&pool, q).await;
    assert_eq!(n, 1, "difference of alpha − archived should yield 1 row");
}

#[tokio::test]
async fn union_all_keeps_duplicates() {
    // Same query unioned-all with itself yields 2× the row count.
    let pool = build_pool().await;
    let drafts1 = QuerySet::<QssoPost>::new().filter("status", "draft");
    let drafts2 = QuerySet::<QssoPost>::new().filter("status", "draft");
    let q = drafts1.union_all(drafts2).compile().unwrap();
    let n = count_via_select(&pool, q).await;
    assert_eq!(
        n, 4,
        "union_all of drafts ∪ drafts should yield 2×2 = 4 rows"
    );
}
