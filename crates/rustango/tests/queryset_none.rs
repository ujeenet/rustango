//! Django-parity #331 — `QuerySet::none()` returns an empty queryset.
//!
//! Verifies that every terminal op short-circuits to the empty
//! result without violating typing or panicking. Hits sqlite live so
//! the `LIMIT 0` / `IS NULL` predicates compose with real SQL.

#![cfg(feature = "sqlite")]

use rustango::core::{Filter, Model as _, Op, SqlValue, WhereExpr};
use rustango::query::QuerySet;
use rustango::sql::Pool;
use rustango::Model;

#[derive(Model, Debug, Clone)]
#[rustango(table = "qsn_post")]
#[allow(dead_code)]
pub struct QsnPost {
    #[rustango(primary_key)]
    id: rustango::Auto<i64>,
    #[rustango(max_length = 200)]
    title: String,
}

async fn build_pool() -> Pool {
    let pool = Pool::connect("sqlite::memory:").await.expect("sqlite pool");
    rustango::sql::raw_execute_pool(
        &pool,
        r#"CREATE TABLE IF NOT EXISTS "qsn_post" (
            "id"    INTEGER PRIMARY KEY AUTOINCREMENT,
            "title" TEXT NOT NULL
        )"#,
        Vec::new(),
    )
    .await
    .expect("create");
    for title in ["A", "B", "C"] {
        rustango::sql::raw_execute_pool(
            &pool,
            r#"INSERT INTO "qsn_post" ("title") VALUES (?)"#,
            vec![SqlValue::String(title.into())],
        )
        .await
        .expect("seed");
    }
    pool
}

#[test]
fn compile_select_forces_limit_zero() {
    let q = QuerySet::<QsnPost>::new().none().compile().unwrap();
    assert_eq!(q.limit, Some(0), "select compile must emit LIMIT 0: {q:?}");
}

#[test]
fn compile_delete_appends_pk_is_null() {
    let q = QuerySet::<QsnPost>::new().none().compile_delete().unwrap();
    // Walk the WHERE looking for an `IS NULL` predicate on the PK column.
    let has_never = where_contains_pk_is_null(&q.where_clause);
    assert!(
        has_never,
        "delete WHERE missing IS NULL guard: {:?}",
        q.where_clause
    );
}

#[test]
fn compile_update_appends_pk_is_null() {
    let q = QuerySet::<QsnPost>::new()
        .none()
        .update()
        .set("title", "renamed")
        .compile()
        .unwrap();
    let has_never = where_contains_pk_is_null(&q.where_clause);
    assert!(
        has_never,
        "update WHERE missing IS NULL guard: {:?}",
        q.where_clause
    );
}

#[test]
fn chained_filters_preserved_alongside_none() {
    // .none() does NOT cancel filters appended before/after — Django's
    // semantic is "still a queryset, just empty". The marker rides
    // independently so a later .all() (if we shipped one) could
    // reasonably resurrect; for v1 we just preserve filters.
    let q = QuerySet::<QsnPost>::new()
        .filter("title", "A")
        .none()
        .compile()
        .unwrap();
    assert_eq!(q.limit, Some(0));
    // Original filter still present in the WHERE.
    let has_filter = matches!(
        &q.where_clause,
        WhereExpr::And(nodes) if nodes.iter().any(|n|
            matches!(n, WhereExpr::Predicate(Filter { column: "title", .. }))
        )
    );
    assert!(
        has_filter,
        "filter dropped after .none(): {:?}",
        q.where_clause
    );
}

#[tokio::test]
async fn live_select_returns_empty_set() {
    let pool = build_pool().await;
    let q = QuerySet::<QsnPost>::new().none().compile().unwrap();
    let scalar_fields: Vec<_> = QsnPost::SCHEMA.scalar_fields().collect();
    let rows = rustango::sql::select_rows_as_json(&pool, &q, &scalar_fields)
        .await
        .expect("select");
    assert!(
        rows.is_empty(),
        "expected empty result, got {} row(s)",
        rows.len()
    );
}

#[test]
fn aggregate_compile_short_circuits() {
    // `.none()` must also short-circuit aggregate paths so callers
    // like `.aggregate().annotate("n", Count(...))` see the empty
    // result without a separate code path.
    use rustango::core::AggregateExpr;
    let q = QuerySet::<QsnPost>::new()
        .none()
        .aggregate()
        .annotate("n", AggregateExpr::Count(None))
        .compile()
        .unwrap();
    let has_never = where_contains_pk_is_null(&q.where_clause);
    assert!(
        has_never,
        "aggregate WHERE missing IS NULL guard: {:?}",
        q.where_clause
    );
    assert_eq!(q.limit, Some(0));
}

#[tokio::test]
async fn live_delete_affects_zero_rows() {
    let pool = build_pool().await;
    let q = QuerySet::<QsnPost>::new().none().compile_delete().unwrap();
    let affected = rustango::sql::delete_pool(&pool, &q).await.expect("delete");
    assert_eq!(affected, 0, "expected zero deletes, got {affected}");
    // All seeded rows still present.
    let surviving = rustango::sql::count_rows_pool(
        &pool,
        &rustango::core::CountQuery {
            model: QsnPost::SCHEMA,
            where_clause: WhereExpr::And(vec![]),
            search: None,
        },
    )
    .await
    .expect("count");
    assert_eq!(surviving, 3, "rows changed after .none().delete()");
}

fn where_contains_pk_is_null(w: &WhereExpr) -> bool {
    match w {
        WhereExpr::Predicate(Filter {
            column,
            op: Op::IsNull,
            value: SqlValue::Bool(true),
        }) => *column == QsnPost::SCHEMA.primary_key().unwrap().column,
        WhereExpr::And(nodes) | WhereExpr::Or(nodes) | WhereExpr::Xor(nodes) => {
            nodes.iter().any(where_contains_pk_is_null)
        }
        WhereExpr::Not(inner) => where_contains_pk_is_null(inner),
        _ => false,
    }
}
