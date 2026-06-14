#![cfg(feature = "sqlite")]
//! Live SQLite test for `QuerySet::where_has(name)` /
//! `QuerySet::where_doesnt_have(name)` — issue #830 slice 1
//! (relation-name → correlated `EXISTS` resolver).
//!
//! Builds on the existing macro-emitted `<name>_exists_expr()` /
//! `<name>_not_exists_expr()` accessors. The QuerySet shortcut
//! resolves the relation via `Model::reverse_relations()` so the
//! caller doesn't need a concrete `self`.

use rustango::core::Model as _;
use rustango::sql::{sqlx, Auto, FetcherPool as _, ForeignKey, Pool};
use rustango::Model;

#[derive(Model, Debug, Clone)]
#[rustango(
    table = "wh_author",
    reverse_has(name = "books", child = "Book", child_fk_column = "author_id",)
)]
#[allow(dead_code)]
pub struct Author {
    #[rustango(primary_key)]
    pub id: Auto<i64>,
    #[rustango(max_length = 40)]
    pub name: String,
}

#[derive(Model, Debug, Clone)]
#[rustango(table = "wh_book")]
#[allow(dead_code)]
pub struct Book {
    #[rustango(primary_key)]
    pub id: Auto<i64>,
    #[rustango(max_length = 80)]
    pub title: String,
    pub author_id: ForeignKey<Author, i64>,
}

async fn make_pool() -> Pool {
    let p = sqlx::SqlitePool::connect("sqlite::memory:")
        .await
        .expect("sqlite memory");
    sqlx::query(
        "CREATE TABLE wh_author (
            id   INTEGER PRIMARY KEY AUTOINCREMENT,
            name TEXT NOT NULL
        )",
    )
    .execute(&p)
    .await
    .unwrap();
    sqlx::query(
        "CREATE TABLE wh_book (
            id        INTEGER PRIMARY KEY AUTOINCREMENT,
            title     TEXT NOT NULL,
            author_id INTEGER NOT NULL
        )",
    )
    .execute(&p)
    .await
    .unwrap();
    p.into()
}

async fn seed(pool: &Pool) {
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
    // Alice has a book; Bob doesn't.
    let mut book = Book {
        id: Auto::default(),
        title: "Alpha".into(),
        author_id: ForeignKey::from(alice_id),
    };
    book.save_pool(pool).await.unwrap();
}

#[tokio::test]
async fn where_has_resolves_relation_name_to_exists() {
    let pool = make_pool().await;
    seed(&pool).await;
    let rows = Author::objects()
        .where_has("books")
        .fetch(&pool)
        .await
        .unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].name, "Alice");
}

#[tokio::test]
async fn where_doesnt_have_resolves_relation_name_to_not_exists() {
    let pool = make_pool().await;
    seed(&pool).await;
    let rows = Author::objects()
        .where_doesnt_have("books")
        .fetch(&pool)
        .await
        .unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].name, "Bob");
}

#[tokio::test]
async fn where_has_unknown_relation_errors_at_compile_time() {
    let pool = make_pool().await;
    seed(&pool).await;
    let err = Author::objects()
        .where_has("nonexistent")
        .fetch(&pool)
        .await
        .unwrap_err();
    let msg = format!("{err}");
    assert!(
        msg.contains("nonexistent"),
        "expected UnknownField for missing relation, got: {msg}"
    );
}

#[test]
fn reverse_relations_runtime_metadata_is_populated() {
    // Author has 1 reverse-has declaration → metadata visible at runtime.
    let rels = Author::reverse_relations();
    assert_eq!(rels.len(), 1);
    assert_eq!(rels[0].name, "books");
    assert_eq!(rels[0].child_schema.table, "wh_book");
    assert_eq!(rels[0].child_fk_column, "author_id");
    assert_eq!(rels[0].self_pk_column, "id");

    // Book has none → empty slice (the trait default).
    assert!(Book::reverse_relations().is_empty());
}
