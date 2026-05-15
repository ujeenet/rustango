#![cfg(feature = "postgres")]
//! Live PG tests for `QuerySet::in_bulk_pool` + `in_bulk_on` (issue #24).
//! Verifies the IN-list filter + HashMap return shape matches Django's
//! `Model.objects.in_bulk(ids, field_name=)`. Skips silently when
//! `DATABASE_URL` is unset.

use std::collections::HashMap;
use std::sync::OnceLock;

use rustango::sql::{sqlx, Auto, Pool};
use rustango::Model;
use tokio::sync::Mutex;

#[derive(Model, Debug, Clone)]
#[rustango(table = "inbulk_book")]
#[allow(dead_code)]
pub struct Book {
    #[rustango(primary_key)]
    pub id: Auto<i64>,
    #[rustango(max_length = 32)]
    pub isbn: String,
    #[rustango(max_length = 64)]
    pub title: String,
}

fn lock() -> &'static Mutex<()> {
    static M: OnceLock<Mutex<()>> = OnceLock::new();
    M.get_or_init(|| Mutex::new(()))
}

async fn fresh_pool() -> Option<Pool> {
    let url = std::env::var("DATABASE_URL").ok()?;
    let pg = sqlx::PgPool::connect(&url).await.ok()?;
    sqlx::query(r#"DROP TABLE IF EXISTS "inbulk_book" CASCADE"#)
        .execute(&pg)
        .await
        .unwrap();
    sqlx::query(
        r#"
        CREATE TABLE "inbulk_book" (
            id BIGSERIAL PRIMARY KEY,
            isbn VARCHAR(32) NOT NULL UNIQUE,
            title VARCHAR(64) NOT NULL
        )
        "#,
    )
    .execute(&pg)
    .await
    .unwrap();
    let pool = Pool::Postgres(pg);
    for (isbn, title) in [
        ("isbn-1", "The Rust Programming Language"),
        ("isbn-2", "Programming Rust"),
        ("isbn-3", "Rust in Action"),
        ("isbn-4", "Zero to Production in Rust"),
    ] {
        let mut b = Book {
            id: Auto::default(),
            isbn: isbn.into(),
            title: title.into(),
        };
        b.insert_pool(&pool).await.unwrap();
    }
    Some(pool)
}

fn pk_of(b: &Book) -> i64 {
    match b.id {
        Auto::Set(v) => v,
        Auto::Unset => unreachable!("fetched row should have Auto::Set PK"),
    }
}

/// Default Django shape — `Book.objects.in_bulk([1, 2, 3])` keyed by PK.
/// Subset of the inserted rows comes back, missing IDs are simply
/// absent from the map (no error).
#[tokio::test]
async fn in_bulk_by_pk_returns_map_keyed_by_id() {
    let _g = lock().lock().await;
    let Some(pool) = fresh_pool().await else {
        return;
    };

    // Insert order gives ids 1..=4. Fetch a 2-of-4 subset plus a
    // non-existent id (999) — the non-existent should be absent.
    let books: HashMap<i64, Book> = Book::objects()
        .in_bulk_pool(Book::id, [1_i64, 3, 999], pk_of, &pool)
        .await
        .unwrap();

    assert_eq!(books.len(), 2, "two real ids, one missing — got {books:?}");
    assert!(books.contains_key(&1));
    assert!(books.contains_key(&3));
    assert!(!books.contains_key(&999));
    assert_eq!(books[&1].isbn, "isbn-1");
    assert_eq!(books[&3].isbn, "isbn-3");
}

/// Django's `in_bulk(ids, field_name='isbn')` — key by a non-PK unique
/// column. Same shape, K = String instead of i64.
#[tokio::test]
async fn in_bulk_by_non_pk_unique_column_keys_on_that_column() {
    let _g = lock().lock().await;
    let Some(pool) = fresh_pool().await else {
        return;
    };

    let books: HashMap<String, Book> = Book::objects()
        .in_bulk_pool(
            Book::isbn,
            ["isbn-2".to_string(), "isbn-4".to_string()],
            |b| b.isbn.clone(),
            &pool,
        )
        .await
        .unwrap();

    assert_eq!(books.len(), 2);
    assert!(books.contains_key("isbn-2"));
    assert!(books.contains_key("isbn-4"));
    assert_eq!(books["isbn-2"].title, "Programming Rust");
}

/// Empty `ids` short-circuits — no SQL is issued, an empty map is
/// returned. Avoids `Op::In` with an empty list (which the SQL writer
/// would reject with `EmptyInList`).
#[tokio::test]
async fn in_bulk_with_empty_ids_returns_empty_map_no_sql() {
    let _g = lock().lock().await;
    let Some(pool) = fresh_pool().await else {
        return;
    };

    let books: HashMap<i64, Book> = Book::objects()
        .in_bulk_pool(Book::id, Vec::<i64>::new(), pk_of, &pool)
        .await
        .unwrap();

    assert!(books.is_empty());
}

/// `in_bulk_pool` composes with prior `.where_()` calls — the IN-list
/// filter AND-joins with whatever the queryset already has. Only
/// matches that pass BOTH the existing WHERE and the IN list come
/// back. Concrete example: filter by title prefix first, then look
/// up by id — missing matches just drop out.
#[tokio::test]
async fn in_bulk_composes_with_prior_where_clauses() {
    use rustango::core::Column as _;
    let _g = lock().lock().await;
    let Some(pool) = fresh_pool().await else {
        return;
    };

    // Pre-filter to books whose title starts with "Rust" — that's
    // ids 3 ("Rust in Action"). Ask for ids 1, 2, 3, 4; only 3
    // survives the pre-filter.
    let books: HashMap<i64, Book> = Book::objects()
        .where_(Book::title.like("Rust%"))
        .in_bulk_pool(Book::id, [1_i64, 2, 3, 4], pk_of, &pool)
        .await
        .unwrap();

    assert_eq!(books.len(), 1, "only id 3 matches title prefix: {books:?}");
    assert!(books.contains_key(&3));
}

/// Tenant-scoped path: `in_bulk_on` takes any sqlx executor (here a
/// raw `&PgPool` for the test, real callers pass `tenant.conn()`).
/// Same semantics as the pool path.
#[tokio::test]
async fn in_bulk_on_uses_executor_directly() {
    let _g = lock().lock().await;
    let Some(pool) = fresh_pool().await else {
        return;
    };
    // With only the `postgres` feature on, `Pool` has a single
    // variant — pattern is irrefutable but the test stays
    // configuration-honest if mysql/sqlite features are ever
    // co-enabled in this binary.
    #[allow(irrefutable_let_patterns)]
    let Pool::Postgres(pg) = &pool
    else {
        unreachable!("test builds with only postgres feature")
    };
    let pg = pg.clone();

    let books: HashMap<i64, Book> = Book::objects()
        .in_bulk_on(Book::id, [2_i64, 4], pk_of, &pg)
        .await
        .unwrap();

    assert_eq!(books.len(), 2);
    assert_eq!(books[&2].isbn, "isbn-2");
    assert_eq!(books[&4].isbn, "isbn-4");
}
