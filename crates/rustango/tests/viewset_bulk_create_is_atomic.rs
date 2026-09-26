//! Bulk create is atomic in the writes, not just the validation (#1403).
//!
//! `docs/viewsets.md` said "validated atomically (one bad element
//! rejects the whole batch)". Validation was atomic; the writes were
//! one `INSERT` each with no enclosing transaction and an early return
//! on the first database error.
//!
//! So a `POST` of ten elements whose fifth violates a unique constraint
//! committed elements 0-4, answered `400 bulk entry 5`, and listed none
//! of the rows it had created. The caller is told the batch failed, five
//! rows exist, and the response contains nothing identifying them. A
//! naive retry then duplicates the first five or fails on element 0.
//!
//! Constraint violations are precisely the class validation cannot
//! decide up front — uniqueness against rows this request has not
//! inserted yet, foreign keys against rows another transaction may have
//! deleted. That is the class that reached the loop, and the class the
//! loop handled worst.
//!
//! Run on all three backends, because the fix is a transaction and
//! transaction semantics are where dialects differ. SQLite always runs;
//! Postgres and MySQL run when their URL is set, and **fail loudly**
//! when it is set but unreachable (#1440).

#![cfg(all(feature = "sqlite", feature = "tenancy"))]

use axum::body::Body;
use axum::http::{header, Method, Request, StatusCode};
use rustango::core::Model as _;
use rustango::sql::{Auto, Pool};
use rustango::viewset::ViewSet;
use rustango::Model;
use tower::ServiceExt;

#[derive(Model, Debug, Clone)]
#[rustango(table = "atomic_widget", display = "label")]
#[allow(dead_code)]
pub struct Widget {
    #[rustango(primary_key)]
    pub id: Auto<i64>,
    /// UNIQUE — the constraint the serializer cannot check, because it
    /// is a property of the table rather than of the payload.
    #[rustango(max_length = 60, unique)]
    pub label: String,
    pub priority: i32,
}

const DDL_SQLITE: &str = r#"CREATE TABLE atomic_widget (
    id       INTEGER PRIMARY KEY AUTOINCREMENT,
    label    TEXT NOT NULL UNIQUE,
    priority INTEGER NOT NULL
)"#;

const DDL_PG: &str = r#"CREATE TABLE atomic_widget (
    id       BIGSERIAL PRIMARY KEY,
    label    VARCHAR(60) NOT NULL UNIQUE,
    priority INTEGER NOT NULL
)"#;

const DDL_MYSQL: &str = r#"CREATE TABLE atomic_widget (
    id       BIGINT NOT NULL AUTO_INCREMENT PRIMARY KEY,
    label    VARCHAR(60) NOT NULL UNIQUE,
    priority INT NOT NULL
)"#;

async fn prepare(pool: &Pool, ddl: &str) {
    rustango::sql::raw_execute_pool(pool, "DROP TABLE IF EXISTS atomic_widget", Vec::new())
        .await
        .expect("drop");
    rustango::sql::raw_execute_pool(pool, ddl, Vec::new())
        .await
        .expect("create");
}

async fn post_json(pool: &Pool, body: &str) -> StatusCode {
    let app = ViewSet::for_model(Widget::SCHEMA).router_pool("/widgets", pool.clone());
    app.oneshot(
        Request::builder()
            .method(Method::POST)
            .uri("/widgets")
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from(body.to_owned()))
            .unwrap(),
    )
    .await
    .unwrap()
    .status()
}

async fn row_count(pool: &Pool) -> i64 {
    // Empty `And` is vacuously true — no WHERE emitted.
    let q =
        rustango::core::CountQuery::new(Widget::SCHEMA, rustango::core::WhereExpr::And(Vec::new()));
    rustango::sql::count_rows_pool(pool, &q)
        .await
        .expect("count")
}

/// The whole issue, on one pool.
///
/// Entry 2 duplicates entry 0's label. The batch must be refused **and
/// leave the table empty** — before the fix, entries 0 and 1 were
/// committed and only the 400 came back.
async fn assert_atomic(pool: &Pool, ddl: &str, backend: &str) {
    prepare(pool, ddl).await;

    let status = post_json(
        pool,
        r#"[{"label":"alpha","priority":1},
            {"label":"beta","priority":2},
            {"label":"alpha","priority":3}]"#,
    )
    .await;
    assert_eq!(
        status,
        StatusCode::BAD_REQUEST,
        "[{backend}] a duplicate label must reject the batch"
    );
    assert_eq!(
        row_count(pool).await,
        0,
        "[{backend}] a rejected batch must leave NOTHING behind — the earlier \
         entries used to commit, so the caller got a 400 over rows that existed"
    );

    // The guard against "refuse everything": a clean batch still lands.
    let status = post_json(
        pool,
        r#"[{"label":"gamma","priority":1},{"label":"delta","priority":2}]"#,
    )
    .await;
    assert_eq!(
        status,
        StatusCode::CREATED,
        "[{backend}] a valid batch must still be created"
    );
    assert_eq!(row_count(pool).await, 2, "[{backend}] both rows land");
}

#[tokio::test]
async fn sqlite_bulk_create_is_atomic() {
    let pool = Pool::connect("sqlite::memory:").await.expect("sqlite");
    assert_atomic(&pool, DDL_SQLITE, "sqlite").await;
}

#[cfg(feature = "postgres")]
#[tokio::test]
async fn postgres_bulk_create_is_atomic() {
    let Ok(url) = std::env::var("DATABASE_URL") else {
        eprintln!("DATABASE_URL not set — skipping the PG arm of the #1403 guard");
        return;
    };
    let pg = sqlx::PgPool::connect(&url)
        .await
        .unwrap_or_else(|e| panic!("DATABASE_URL is set but unreachable ({url}): {e}"));
    assert_atomic(&Pool::Postgres(pg), DDL_PG, "postgres").await;
}

#[cfg(feature = "mysql")]
#[tokio::test]
async fn mysql_bulk_create_is_atomic() {
    let Ok(url) = std::env::var("MYSQL_TEST_URL") else {
        eprintln!("MYSQL_TEST_URL not set — skipping the MySQL arm of the #1403 guard");
        return;
    };
    let my = sqlx::MySqlPool::connect(&url)
        .await
        .unwrap_or_else(|e| panic!("MYSQL_TEST_URL is set but unreachable ({url}): {e}"));
    assert_atomic(&Pool::Mysql(my), DDL_MYSQL, "mysql").await;
}
