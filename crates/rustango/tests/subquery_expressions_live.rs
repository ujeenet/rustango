#![cfg(feature = "postgres")]
//! Live PG tests for `Subquery` / `Exists` / `OuterRef` (issue #5).
//! The emission tests pin the SQL strings; this pins the runtime
//! semantics — actual rows in / actual rows out against a running
//! database.
//!
//! Skips silently when `DATABASE_URL` is unset.

use std::sync::OnceLock;

use rustango::core::subquery::{exists, not_exists, outer_ref};
use rustango::core::Column as _;
use rustango::sql::{sqlx, Auto, Fetcher, Updater};
use rustango::Model;
use tokio::sync::Mutex;

fn live_lock() -> &'static Mutex<()> {
    static M: OnceLock<Mutex<()>> = OnceLock::new();
    M.get_or_init(|| Mutex::new(()))
}

#[derive(Model, Debug, Clone)]
#[rustango(table = "sql_author")]
#[allow(dead_code)]
pub struct Author {
    #[rustango(primary_key)]
    pub id: Auto<i64>,
    #[rustango(max_length = 100)]
    pub name: String,
    pub book_count: i64,
}

#[derive(Model, Debug, Clone)]
#[rustango(table = "sql_book")]
#[allow(dead_code)]
pub struct Book {
    #[rustango(primary_key)]
    pub id: Auto<i64>,
    pub author_id: i64,
    #[rustango(max_length = 200)]
    pub title: String,
    #[rustango(max_length = 20)]
    pub status: String,
    pub pages: i64,
}

async fn pool() -> Option<sqlx::PgPool> {
    let url = std::env::var("DATABASE_URL").ok()?;
    sqlx::PgPool::connect(&url).await.ok()
}

async fn fresh(pool: &sqlx::PgPool) {
    sqlx::query(r#"DROP TABLE IF EXISTS "sql_book" CASCADE"#)
        .execute(pool)
        .await
        .unwrap();
    sqlx::query(r#"DROP TABLE IF EXISTS "sql_author" CASCADE"#)
        .execute(pool)
        .await
        .unwrap();
    sqlx::query(
        r#"CREATE TABLE "sql_author" (
            "id" BIGSERIAL PRIMARY KEY,
            "name" VARCHAR(100) NOT NULL,
            "book_count" BIGINT NOT NULL DEFAULT 0
        )"#,
    )
    .execute(pool)
    .await
    .unwrap();
    sqlx::query(
        r#"CREATE TABLE "sql_book" (
            "id" BIGSERIAL PRIMARY KEY,
            "author_id" BIGINT NOT NULL REFERENCES "sql_author"("id"),
            "title" VARCHAR(200) NOT NULL,
            "status" VARCHAR(20) NOT NULL,
            "pages" BIGINT NOT NULL DEFAULT 0
        )"#,
    )
    .execute(pool)
    .await
    .unwrap();

    // 3 authors: Alice (published book), Bob (draft book), Carol (no book).
    sqlx::query(
        r#"INSERT INTO "sql_author" ("id", "name") VALUES (1, 'Alice'), (2, 'Bob'), (3, 'Carol')"#,
    )
    .execute(pool)
    .await
    .unwrap();
    sqlx::query(r#"INSERT INTO "sql_book" ("author_id", "title", "status", "pages") VALUES (1, 'Alpha', 'published', 200), (2, 'Bravo', 'draft', 50)"#)
        .execute(pool)
        .await
        .unwrap();
}

/// Issue #4 acceptance: "authors with no books" via NOT EXISTS — the
/// canonical anti-join shape. Carol is the only row that should match.
#[tokio::test]
async fn not_exists_finds_authors_with_no_books() {
    let _g = live_lock().lock().await;
    let Some(pool) = pool().await else {
        return;
    };
    fresh(&pool).await;

    let inner = Book::objects()
        .where_(Book::author_id.eq_expr(outer_ref("id")))
        .compile()
        .unwrap();
    // No-op UPDATE that we can prove via post-conditions: tag
    // authors-with-no-books by setting their book_count to -1.
    Author::objects()
        .where_raw(not_exists(inner))
        .update()
        .set("book_count", -1_i64)
        .execute(&pool)
        .await
        .unwrap();

    let rows: Vec<Author> = Author::objects()
        .order_by(&[("id", false)])
        .fetch(&pool)
        .await
        .unwrap();
    // Alice + Bob keep 0 (they have books); Carol jumps to -1.
    assert_eq!(rows[0].book_count, 0, "Alice has a book");
    assert_eq!(rows[1].book_count, 0, "Bob has a book");
    assert_eq!(rows[2].book_count, -1, "Carol has no books");

    cleanup(&pool).await;
}

/// EXISTS — positive form. Tag authors who have at least one
/// PUBLISHED book.
#[tokio::test]
async fn exists_finds_authors_with_published_books() {
    let _g = live_lock().lock().await;
    let Some(pool) = pool().await else {
        return;
    };
    fresh(&pool).await;

    let inner = Book::objects()
        .where_(Book::author_id.eq_expr(outer_ref("id")))
        .where_(Book::status.eq("published"))
        .compile()
        .unwrap();
    Author::objects()
        .where_raw(exists(inner))
        .update()
        .set("book_count", 99_i64)
        .execute(&pool)
        .await
        .unwrap();

    let rows: Vec<Author> = Author::objects()
        .order_by(&[("id", false)])
        .fetch(&pool)
        .await
        .unwrap();
    // Only Alice has a published book.
    assert_eq!(rows[0].book_count, 99, "Alice has published");
    assert_eq!(rows[1].book_count, 0, "Bob only has draft");
    assert_eq!(rows[2].book_count, 0, "Carol has no books");

    cleanup(&pool).await;
}

/// EXISTS-as-correlated-anti-join, alternate phrasing: tag authors
/// whose book has > 100 pages. This is the pattern users will reach
/// for instead of `IN (SELECT …)` until projection-narrowing ships —
/// EXISTS doesn't care how many columns the inner SELECT projects.
#[tokio::test]
async fn exists_filters_by_correlated_inner_predicate() {
    let _g = live_lock().lock().await;
    let Some(pool) = pool().await else {
        return;
    };
    fresh(&pool).await;

    let inner = Book::objects()
        .where_(Book::author_id.eq_expr(outer_ref("id")))
        .where_(Book::pages.gt(100_i64))
        .compile()
        .unwrap();
    Author::objects()
        .where_raw(exists(inner))
        .update()
        .set("book_count", 77_i64)
        .execute(&pool)
        .await
        .unwrap();

    let rows: Vec<Author> = Author::objects()
        .order_by(&[("id", false)])
        .fetch(&pool)
        .await
        .unwrap();
    assert_eq!(rows[0].book_count, 77, "Alice has a 200-page book");
    assert_eq!(rows[1].book_count, 0, "Bob's book is 50 pages");
    assert_eq!(rows[2].book_count, 0, "Carol has no books");

    cleanup(&pool).await;
}

/// Composition: `EXISTS` inside an OR with another predicate.
#[tokio::test]
async fn exists_composes_with_or_in_outer_where() {
    let _g = live_lock().lock().await;
    let Some(pool) = pool().await else {
        return;
    };
    fresh(&pool).await;

    // Tag rows where name='Carol' OR EXISTS(any-book-with-pages>100).
    // Alice matches via EXISTS, Carol matches via name. Bob doesn't.
    use rustango::core::WhereExpr;
    let inner = Book::objects()
        .where_(Book::author_id.eq_expr(outer_ref("id")))
        .where_(Book::pages.gt(100_i64))
        .compile()
        .unwrap();
    Author::objects()
        .where_raw(WhereExpr::Or(vec![
            Author::name.eq("Carol").into(),
            exists(inner),
        ]))
        .update()
        .set("book_count", 7_i64)
        .execute(&pool)
        .await
        .unwrap();

    let rows: Vec<Author> = Author::objects()
        .order_by(&[("id", false)])
        .fetch(&pool)
        .await
        .unwrap();
    assert_eq!(rows[0].book_count, 7, "Alice via EXISTS");
    assert_eq!(rows[1].book_count, 0, "Bob: no published, not Carol");
    assert_eq!(rows[2].book_count, 7, "Carol via name");

    cleanup(&pool).await;
}

async fn cleanup(pool: &sqlx::PgPool) {
    sqlx::query(r#"DROP TABLE IF EXISTS "sql_book" CASCADE"#)
        .execute(pool)
        .await
        .unwrap();
    sqlx::query(r#"DROP TABLE IF EXISTS "sql_author" CASCADE"#)
        .execute(pool)
        .await
        .unwrap();
}
