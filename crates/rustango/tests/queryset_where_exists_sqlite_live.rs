#![cfg(feature = "sqlite")]
//! Live SQLite test for `QuerySet::where_exists(subquery)` /
//! `QuerySet::where_not_exists(subquery)` — Eloquent
//! `Builder::whereExists` / `whereNotExists` parity. Routes through
//! the existing `subquery::exists` / `not_exists` free functions but
//! drops the `where_raw(...)` wrapper from call sites.

use rustango::core::subquery::outer_ref;
use rustango::core::Column as _;
use rustango::sql::{sqlx, Auto, FetcherPool as _, Pool};
use rustango::Model;

#[derive(Model, Debug, Clone)]
#[rustango(table = "we_author")]
#[allow(dead_code)]
pub struct Author {
    #[rustango(primary_key)]
    pub id: Auto<i64>,
    #[rustango(max_length = 40)]
    pub name: String,
}

#[derive(Model, Debug, Clone)]
#[rustango(table = "we_book")]
#[allow(dead_code)]
pub struct Book {
    #[rustango(primary_key)]
    pub id: Auto<i64>,
    pub author_id: i64,
    #[rustango(max_length = 40)]
    pub title: String,
}

async fn make_pool() -> Pool {
    let p = sqlx::SqlitePool::connect("sqlite::memory:")
        .await
        .expect("sqlite memory");
    sqlx::query(
        "CREATE TABLE we_author (
            id   INTEGER PRIMARY KEY AUTOINCREMENT,
            name TEXT NOT NULL
        )",
    )
    .execute(&p)
    .await
    .unwrap();
    sqlx::query(
        "CREATE TABLE we_book (
            id        INTEGER PRIMARY KEY AUTOINCREMENT,
            author_id INTEGER NOT NULL,
            title     TEXT NOT NULL
        )",
    )
    .execute(&p)
    .await
    .unwrap();
    p.into()
}

async fn seed(pool: &Pool) {
    // Alice has 1 book; Bob has 0.
    let mut alice = Author {
        id: Auto::default(),
        name: "Alice".into(),
    };
    alice.save_pool(pool).await.unwrap();
    let alice_id = *alice.id.get().unwrap();

    let mut bob = Author {
        id: Auto::default(),
        name: "Bob".into(),
    };
    bob.save_pool(pool).await.unwrap();

    let mut book = Book {
        id: Auto::default(),
        author_id: alice_id,
        title: "Alpha".into(),
    };
    book.save_pool(pool).await.unwrap();
}

#[tokio::test]
async fn where_exists_finds_authors_with_books() {
    let pool = make_pool().await;
    seed(&pool).await;
    let inner = Book::objects()
        .where_(Book::author_id.eq_expr(outer_ref("id")))
        .compile()
        .unwrap();
    let rows = Author::objects()
        .where_exists(inner)
        .fetch(&pool)
        .await
        .unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].name, "Alice");
}

#[tokio::test]
async fn where_not_exists_finds_authors_with_no_books() {
    let pool = make_pool().await;
    seed(&pool).await;
    let inner = Book::objects()
        .where_(Book::author_id.eq_expr(outer_ref("id")))
        .compile()
        .unwrap();
    let rows = Author::objects()
        .where_not_exists(inner)
        .fetch(&pool)
        .await
        .unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].name, "Bob");
}
