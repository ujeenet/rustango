#![cfg(feature = "sqlite")]
//! Live SQLite test for `QuerySet::where_has_filter(name, inner)` /
//! `QuerySet::where_doesnt_have_filter(name, inner)` — issue #830
//! slice 2 (whereHas with inner predicate).

use rustango::sql::{sqlx, Auto, FetcherPool as _, ForeignKey, Pool};
use rustango::Model;

#[derive(Model, Debug, Clone)]
#[rustango(
    table = "whf_author",
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
#[rustango(table = "whf_book")]
#[allow(dead_code)]
pub struct Book {
    #[rustango(primary_key)]
    pub id: Auto<i64>,
    #[rustango(max_length = 80)]
    pub title: String,
    pub author_id: ForeignKey<Author, i64>,
    pub published: bool,
}

#[derive(Model, Debug, Clone)]
#[rustango(table = "whf_other")]
#[allow(dead_code)]
pub struct Other {
    #[rustango(primary_key)]
    pub id: Auto<i64>,
}

async fn make_pool() -> Pool {
    let p = sqlx::SqlitePool::connect("sqlite::memory:")
        .await
        .expect("sqlite memory");
    sqlx::query(
        "CREATE TABLE whf_author (
            id   INTEGER PRIMARY KEY AUTOINCREMENT,
            name TEXT NOT NULL
        )",
    )
    .execute(&p)
    .await
    .unwrap();
    sqlx::query(
        "CREATE TABLE whf_book (
            id        INTEGER PRIMARY KEY AUTOINCREMENT,
            title     TEXT NOT NULL,
            author_id INTEGER NOT NULL,
            published INTEGER NOT NULL
        )",
    )
    .execute(&p)
    .await
    .unwrap();
    sqlx::query("CREATE TABLE whf_other (id INTEGER PRIMARY KEY AUTOINCREMENT)")
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
    let bob_id = *bob.id.get().unwrap();
    let mut carol = Author {
        id: Auto::default(),
        name: "Carol".into(),
    };
    carol.save_pool(pool).await.unwrap();

    // Alice has a published book.
    Book {
        id: Auto::default(),
        title: "Alpha".into(),
        author_id: ForeignKey::from(alice_id),
        published: true,
    }
    .save_pool(pool)
    .await
    .unwrap();
    // Bob has only a draft.
    Book {
        id: Auto::default(),
        title: "Beta".into(),
        author_id: ForeignKey::from(bob_id),
        published: false,
    }
    .save_pool(pool)
    .await
    .unwrap();
    // Carol has no books.
}

#[tokio::test]
async fn where_has_filter_narrows_to_authors_with_matching_child_rows() {
    let pool = make_pool().await;
    seed(&pool).await;
    let inner = Book::objects().filter("published", true).compile().unwrap();
    let rows = Author::objects()
        .where_has_filter("books", inner)
        .fetch_pool(&pool)
        .await
        .unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].name, "Alice");
}

#[tokio::test]
async fn where_doesnt_have_filter_finds_authors_without_matching_child_rows() {
    let pool = make_pool().await;
    seed(&pool).await;
    // "Authors with no published book" — Bob (draft only) and Carol (none).
    let inner = Book::objects().filter("published", true).compile().unwrap();
    let mut names: Vec<String> = Author::objects()
        .where_doesnt_have_filter("books", inner)
        .fetch_pool(&pool)
        .await
        .unwrap()
        .into_iter()
        .map(|a| a.name)
        .collect();
    names.sort();
    assert_eq!(names, vec!["Bob", "Carol"]);
}

#[tokio::test]
async fn where_has_filter_with_unknown_relation_errors() {
    let pool = make_pool().await;
    seed(&pool).await;
    let inner = Book::objects().compile().unwrap();
    let err = Author::objects()
        .where_has_filter("nonexistent", inner)
        .fetch_pool(&pool)
        .await
        .unwrap_err();
    assert!(format!("{err}").contains("nonexistent"));
}

#[tokio::test]
async fn where_has_filter_with_wrong_child_model_errors() {
    let pool = make_pool().await;
    seed(&pool).await;
    // Inner queryset is built on `Other`, but the `books` relation
    // is declared against `Book`. Guard against silent SQL mismatch.
    let wrong_inner = Other::objects().compile().unwrap();
    let err = Author::objects()
        .where_has_filter("books", wrong_inner)
        .fetch_pool(&pool)
        .await
        .unwrap_err();
    let msg = format!("{err}");
    assert!(
        msg.contains("model mismatch") || msg.contains("books"),
        "expected mismatch error, got: {msg}"
    );
}
